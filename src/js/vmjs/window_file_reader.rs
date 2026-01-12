//! Minimal `FileReader` implementation for `vm-js` Window realms.
//!
//! This is a spec-shaped MVP intended to unblock common upload/preview code paths that expect
//! `FileReader` to exist.
//!
//! ## Scheduling model
//!
//! Real browsers run `FileReader` asynchronously. In FastRender's `vm-js` embedding we implement
//! "async-ish" completion by scheduling a microtask via the internal `__fastrender_queue_microtask`
//! binding installed by `window_timers`.
//!
//! If that internal `queueMicrotask` hook is missing or cannot be used (e.g. timers not installed /
//! no active `EventLoop`), we currently throw a clear `TypeError` instead of silently completing
//! synchronously.

use base64::engine::general_purpose;
use base64::Engine as _;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use vm_js::{
  GcObject, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind,
  Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

use crate::js::window_blob::clone_blob_data_for_fetch;
use crate::js::window_timers::INTERNAL_QUEUE_MICROTASK_KEY;

const REALM_ID_SLOT: usize = 0;
const GLOBAL_SLOT: usize = 1;

const CALLBACK_REALM_ID_SLOT: usize = 0;
const CALLBACK_READER_SLOT: usize = 1;
const CALLBACK_SEQ_SLOT: usize = 2;
const CALLBACK_ACTION_SLOT: usize = 3;

const LISTENERS_KEY: &str = "__fastrender_file_reader_listeners";

const FILE_READER_EMPTY: u8 = 0;
const FILE_READER_LOADING: u8 = 1;
const FILE_READER_DONE: u8 = 2;

const ACTION_READ: u8 = 0;
const ACTION_ABORT: u8 = 1;

const FILE_READER_EVENT_TYPE_MAX_BYTES: usize = 128;
const FILE_READER_QUEUE_MICROTASK_ERROR: &str =
  "FileReader requires window timers (queueMicrotask) with an active EventLoop";

const MAX_DATA_URL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
enum PendingRead {
  Text { bytes: Vec<u8> },
  ArrayBuffer { bytes: Vec<u8> },
  DataUrl { bytes: Vec<u8>, mime: String },
}

#[derive(Clone, Debug)]
enum FileReaderResult {
  Null,
  Text(String),
  ArrayBuffer(Vec<u8>),
  DataUrl(String),
}

impl Default for FileReaderResult {
  fn default() -> Self {
    Self::Null
  }
}

#[derive(Debug)]
struct FileReaderState {
  ready_state: u8,
  read_seq: u64,
  pending: Option<PendingRead>,
  result: FileReaderResult,
  error: Option<String>,
}

impl Default for FileReaderState {
  fn default() -> Self {
    Self {
      ready_state: FILE_READER_EMPTY,
      read_seq: 0,
      pending: None,
      result: FileReaderResult::Null,
      error: None,
    }
  }
}

#[derive(Default)]
struct FileReaderRegistry {
  realms: HashMap<RealmId, FileReaderRealmState>,
}

struct FileReaderRealmState {
  file_reader_proto: GcObject,
  readers: HashMap<WeakGcObject, FileReaderState>,
  last_gc_runs: u64,
  microtask_call_id: NativeFunctionId,
}

static REGISTRY: OnceLock<Mutex<FileReaderRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<FileReaderRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(FileReaderRegistry::default()))
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  data_desc(value, false)
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn get_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn realm_id_from_slot(value: Value) -> Option<RealmId> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() || n < 0.0 {
    return None;
  }
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(RealmId::from_raw(raw))
}

fn realm_id_for_binding_call(vm: &Vm, scope: &Scope<'_>, callee: GcObject) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }
  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "FileReader bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

fn global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(GLOBAL_SLOT).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "FileReader binding missing global object native slot",
    )),
  }
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut FileReaderRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "FileReader bindings used before install_window_file_reader_bindings",
    ))?;

  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn require_file_reader(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("FileReader: illegal invocation"));
  };
  with_realm_state_mut(vm, scope, callee, |state| {
    state
      .readers
      .contains_key(&WeakGcObject::from(obj))
      .then_some(obj)
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))
  })
}

fn get_or_create_listener_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  listeners_obj: GcObject,
  event_type: &str,
) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, event_type)?;
  let existing = scope
    .heap()
    .object_get_own_data_property_value(listeners_obj, &key)?
    .unwrap_or(Value::Undefined);
  match existing {
    Value::Object(obj) => Ok(obj),
    _ => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let arr = scope.alloc_array(0)?;
      scope.push_root(Value::Object(arr))?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      scope.define_property(listeners_obj, key, data_desc(Value::Object(arr), true))?;
      Ok(arr)
    }
  }
}

fn array_length(scope: &mut Scope<'_>, obj: GcObject) -> Result<usize, VmError> {
  let len_key = alloc_key(scope, "length")?;
  let len_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &len_key)?
    .unwrap_or(Value::Undefined);
  let Value::Number(n) = len_val else {
    return Err(VmError::TypeError("array length is not a number"));
  };
  if !n.is_finite() || n < 0.0 || n > (u32::MAX as f64) {
    return Err(VmError::TypeError("array length out of range"));
  }
  Ok(n as usize)
}

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: vm_js::GcString,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  use std::char::decode_utf16;
  let js = heap.get_string(handle)?;
  let code_units_len = js.len_code_units();
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(error));
  }
  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;
  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(VmError::TypeError(error));
    }
    out.push(ch);
    out_len = next_len;
  }
  Ok(out)
}

fn to_rust_string_limited(
  heap: &mut Heap,
  value: Value,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let s = heap.to_string(value)?;
  js_string_to_rust_string_limited(heap, s, max_bytes, error)
}

fn slot_number_to_u64(value: Value) -> Option<u64> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() || n < 0.0 || n > (u64::MAX as f64) {
    return None;
  }
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(raw)
}

fn slot_number_to_u8(value: Value) -> Option<u8> {
  slot_number_to_u64(value).and_then(|v| u8::try_from(v).ok())
}

fn schedule_file_reader_microtask(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  microtask_call_id: NativeFunctionId,
  reader_obj: GcObject,
  seq: u64,
  action: u8,
) -> Result<(), VmError> {
  let global = global_from_callee(scope, callee)?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Look up the internal `queueMicrotask` hook installed by `window_timers`.
  let queue_microtask = {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(global))?;
    let key = alloc_key(&mut scope, INTERNAL_QUEUE_MICROTASK_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)?
      .unwrap_or(Value::Undefined)
  };

  if !scope.heap().is_callable(queue_microtask).unwrap_or(false) {
    return Err(VmError::TypeError(FILE_READER_QUEUE_MICROTASK_ERROR));
  }

  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  // Create a per-read callback function that captures the reader + seq in native slots.
  let cb_name = scope.alloc_string("__fastrender_file_reader_microtask")?;
  scope.push_root(Value::String(cb_name))?;
  scope.push_root(Value::Object(reader_obj))?;
  let slots = [
    Value::Number(realm_id.to_raw() as f64),
    Value::Object(reader_obj),
    Value::Number(seq as f64),
    Value::Number(action as f64),
  ];
  let cb = scope.alloc_native_function_with_slots(microtask_call_id, None, cb_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(cb, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(cb))?;

  // Root `queue_microtask` while calling it (the call may GC).
  scope.push_root(queue_microtask)?;
  let res = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    queue_microtask,
    Value::Object(global),
    &[Value::Object(cb)],
  );

  match res {
    Ok(_) => Ok(()),
    Err(VmError::TypeError(_)) => Err(VmError::TypeError(FILE_READER_QUEUE_MICROTASK_ERROR)),
    Err(err) => Err(err),
  }
}

fn dispatch_file_reader_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  reader_obj: GcObject,
  event_type: &str,
) -> Result<(), VmError> {
  // Build a minimal event object.
  let event_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(event_obj))?;
  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;
  set_data_prop(scope, event_obj, "type", Value::String(type_s), true)?;
  set_data_prop(scope, event_obj, "target", Value::Object(reader_obj), true)?;
  set_data_prop(
    scope,
    event_obj,
    "currentTarget",
    Value::Object(reader_obj),
    true,
  )?;

  // Event handler property (`onload`, ...).
  let handler_prop = match event_type {
    "loadstart" => "onloadstart",
    "progress" => "onprogress",
    "load" => "onload",
    "error" => "onerror",
    "abort" => "onabort",
    "loadend" => "onloadend",
    other => {
      let mut s = String::with_capacity("on".len() + other.len());
      s.push_str("on");
      s.push_str(other);
      let key = alloc_key(scope, &s)?;
      let value = vm.get_with_host_and_hooks(host, scope, hooks, reader_obj, key)?;
      if scope.heap().is_callable(value).unwrap_or(false) {
        let _ = vm.call_with_host_and_hooks(
          host,
          scope,
          hooks,
          value,
          Value::Object(reader_obj),
          &[Value::Object(event_obj)],
        )?;
      }
      ""
    }
  };

  if !handler_prop.is_empty() {
    let key = alloc_key(scope, handler_prop)?;
    let value = vm.get_with_host_and_hooks(host, scope, hooks, reader_obj, key)?;
    if scope.heap().is_callable(value).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        value,
        Value::Object(reader_obj),
        &[Value::Object(event_obj)],
      )?;
    }
  }

  // Listener list from `addEventListener`.
  let listeners_val = get_data_prop(scope, reader_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Ok(());
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let key = alloc_key(scope, event_type)?;
  let Some(Value::Object(arr)) =
    scope
      .heap()
      .object_get_own_data_property_value(listeners_obj, &key)?
  else {
    return Ok(());
  };
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  for idx in 0..len {
    let k = alloc_key(scope, &idx.to_string())?;
    let listener = scope
      .heap()
      .object_get_own_data_property_value(arr, &k)?
      .unwrap_or(Value::Undefined);
    if scope.heap().is_callable(listener).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        listener,
        Value::Object(reader_obj),
        &[Value::Object(event_obj)],
      )?;
    }
  }

  Ok(())
}

fn file_reader_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("FileReader constructor requires 'new'"))
}

fn file_reader_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.file_reader_proto))?;
  scope.push_root(Value::Object(proto))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  let listeners = scope.alloc_object()?;
  scope.push_root(Value::Object(listeners))?;
  set_data_prop(scope, obj, LISTENERS_KEY, Value::Object(listeners), false)?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state
      .readers
      .insert(WeakGcObject::from(obj), FileReaderState::default());
    Ok(())
  })?;

  Ok(Value::Object(obj))
}

fn file_reader_ready_state_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_file_reader(vm, scope, callee, this)?;
  let ready_state = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .readers
      .get(&WeakGcObject::from(obj))
      .map(|s| s.ready_state)
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))
  })?;
  Ok(Value::Number(ready_state as f64))
}

fn file_reader_result_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_file_reader(vm, scope, callee, this)?;
  let result = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .readers
      .get(&WeakGcObject::from(obj))
      .map(|s| s.result.clone())
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))
  })?;

  match result {
    FileReaderResult::Null => Ok(Value::Null),
    FileReaderResult::Text(s) | FileReaderResult::DataUrl(s) => {
      let js = scope.alloc_string(&s)?;
      scope.push_root(Value::String(js))?;
      Ok(Value::String(js))
    }
    FileReaderResult::ArrayBuffer(bytes) => {
      let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
        "FileReader.result requires intrinsics (create a Realm first)",
      ))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      Ok(Value::Object(ab))
    }
  }
}

fn file_reader_error_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_file_reader(vm, scope, callee, this)?;
  let error = with_realm_state_mut(vm, scope, callee, |state| {
    state
      .readers
      .get(&WeakGcObject::from(obj))
      .map(|s| s.error.clone())
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))
  })?;
  match error {
    None => Ok(Value::Null),
    Some(s) => {
      let js = scope.alloc_string(&s)?;
      scope.push_root(Value::String(js))?;
      Ok(Value::String(js))
    }
  }
}

fn start_read(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  reader_obj: GcObject,
  pending: PendingRead,
) -> Result<(), VmError> {
  let (seq, microtask_call_id) = with_realm_state_mut(vm, scope, callee, |state| {
    let reader = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))?;
    if reader.ready_state == FILE_READER_LOADING {
      return Err(VmError::TypeError("FileReader is already loading"));
    }
    reader.ready_state = FILE_READER_LOADING;
    reader.read_seq = reader.read_seq.saturating_add(1);
    reader.result = FileReaderResult::Null;
    reader.error = None;
    reader.pending = Some(pending);
    Ok((reader.read_seq, state.microtask_call_id))
  })?;

  let schedule_result = schedule_file_reader_microtask(
    vm,
    scope,
    host,
    hooks,
    callee,
    microtask_call_id,
    reader_obj,
    seq,
    ACTION_READ,
  );

  if let Err(err) = schedule_result {
    // Ensure we don't strand the reader in LOADING if scheduling failed.
    let _ = with_realm_state_mut(vm, scope, callee, |state| {
      if let Some(reader) = state.readers.get_mut(&WeakGcObject::from(reader_obj)) {
        if reader.read_seq == seq {
          reader.ready_state = FILE_READER_EMPTY;
          reader.pending = None;
          reader.result = FileReaderResult::Null;
          reader.error = None;
        }
      }
      Ok(())
    });
    return Err(err);
  }

  Ok(())
}

fn file_reader_read_as_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let blob_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(blob) = clone_blob_data_for_fetch(vm, scope.heap(), blob_val)? else {
    return Err(VmError::TypeError("FileReader.readAsText expects a Blob"));
  };

  start_read(
    vm,
    scope,
    host,
    hooks,
    callee,
    reader_obj,
    PendingRead::Text { bytes: blob.bytes },
  )?;
  Ok(Value::Undefined)
}

fn file_reader_read_as_array_buffer_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let blob_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(blob) = clone_blob_data_for_fetch(vm, scope.heap(), blob_val)? else {
    return Err(VmError::TypeError(
      "FileReader.readAsArrayBuffer expects a Blob",
    ));
  };

  start_read(
    vm,
    scope,
    host,
    hooks,
    callee,
    reader_obj,
    PendingRead::ArrayBuffer { bytes: blob.bytes },
  )?;
  Ok(Value::Undefined)
}

fn file_reader_read_as_data_url_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let blob_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(blob) = clone_blob_data_for_fetch(vm, scope.heap(), blob_val)? else {
    return Err(VmError::TypeError(
      "FileReader.readAsDataURL expects a Blob",
    ));
  };

  let crate::js::window_blob::BlobData { bytes, r#type } = blob;
  let mime = if r#type.is_empty() {
    "application/octet-stream".to_string()
  } else {
    r#type
  };

  // Enforce a conservative bound on the encoded output (includes the `data:` prefix and mime).
  let base64_len = ((bytes.len() + 2) / 3)
    .checked_mul(4)
    .ok_or(VmError::OutOfMemory)?;
  let prefix_len = "data:".len()
    .checked_add(mime.len())
    .and_then(|v| v.checked_add(";base64,".len()))
    .ok_or(VmError::OutOfMemory)?;
  let total_len = prefix_len
    .checked_add(base64_len)
    .ok_or(VmError::OutOfMemory)?;
  if total_len > MAX_DATA_URL_BYTES {
    return Err(VmError::TypeError("FileReader.readAsDataURL result is too large"));
  }

  start_read(
    vm,
    scope,
    host,
    hooks,
    callee,
    reader_obj,
    PendingRead::DataUrl {
      bytes,
      mime,
    },
  )?;
  Ok(Value::Undefined)
}

fn file_reader_abort_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let scheduled = with_realm_state_mut(vm, scope, callee, |state| {
    let reader = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))?;
    if reader.ready_state != FILE_READER_LOADING {
      return Ok(None);
    }
    reader.read_seq = reader.read_seq.saturating_add(1);
    reader.ready_state = FILE_READER_DONE;
    reader.result = FileReaderResult::Null;
    reader.error = None;
    reader.pending = None;
    Ok(Some((reader.read_seq, state.microtask_call_id)))
  })?;

  let Some((seq, microtask_call_id)) = scheduled else {
    return Ok(Value::Undefined);
  };

  schedule_file_reader_microtask(
    vm,
    scope,
    host,
    hooks,
    callee,
    microtask_call_id,
    reader_obj,
    seq,
    ACTION_ABORT,
  )?;
  Ok(Value::Undefined)
}

fn file_reader_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let event_type_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);

  if matches!(listener, Value::Undefined | Value::Null) {
    return Ok(Value::Undefined);
  }
  if !scope.heap().is_callable(listener).unwrap_or(false) {
    return Ok(Value::Undefined);
  }

  let event_type = to_rust_string_limited(
    scope.heap_mut(),
    event_type_val,
    FILE_READER_EVENT_TYPE_MAX_BYTES,
    "FileReader event type exceeds maximum length",
  )?;

  let listeners_val = get_data_prop(scope, reader_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Err(VmError::InvariantViolation("FileReader listener registry missing"));
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let arr = get_or_create_listener_array(vm, scope, listeners_obj, &event_type)?;
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  for idx in 0..len {
    let key = alloc_key(scope, &idx.to_string())?;
    let existing = scope
      .heap()
      .object_get_own_data_property_value(arr, &key)?
      .unwrap_or(Value::Undefined);
    if existing == listener {
      return Ok(Value::Undefined);
    }
  }

  let idx = len;
  scope.push_root(listener)?;
  let key = alloc_key(scope, &idx.to_string())?;
  scope.define_property(arr, key, data_desc(listener, true))?;

  Ok(Value::Undefined)
}

fn file_reader_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let event_type_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);

  let event_type = to_rust_string_limited(
    scope.heap_mut(),
    event_type_val,
    FILE_READER_EVENT_TYPE_MAX_BYTES,
    "FileReader event type exceeds maximum length",
  )?;

  let listeners_val = get_data_prop(scope, reader_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Ok(Value::Undefined);
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let key = alloc_key(scope, &event_type)?;
  let Some(Value::Object(arr)) =
    scope
      .heap()
      .object_get_own_data_property_value(listeners_obj, &key)?
  else {
    return Ok(Value::Undefined);
  };
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  let mut removed = false;
  let mut remaining: Vec<Value> = Vec::new();
  remaining.try_reserve(len).map_err(|_| VmError::OutOfMemory)?;
  for idx in 0..len {
    let k = alloc_key(scope, &idx.to_string())?;
    let v = scope
      .heap()
      .object_get_own_data_property_value(arr, &k)?
      .unwrap_or(Value::Undefined);
    if !removed && v == listener {
      removed = true;
      continue;
    }
    remaining.push(v);
  }

  if !removed {
    return Ok(Value::Undefined);
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let new_arr = scope.alloc_array(remaining.len())?;
  scope.push_root(Value::Object(new_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(new_arr, Some(intr.array_prototype()))?;

  for (idx, v) in remaining.into_iter().enumerate() {
    scope.push_root(v)?;
    let k = alloc_key(scope, &idx.to_string())?;
    scope.define_property(new_arr, k, data_desc(v, true))?;
  }

  let key = alloc_key(scope, &event_type)?;
  scope.define_property(listeners_obj, key, data_desc(Value::Object(new_arr), true))?;
  Ok(Value::Undefined)
}

fn file_reader_dispatch_event_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_file_reader(vm, scope, callee, this)?;
  let event_val = args.get(0).copied().unwrap_or(Value::Undefined);

  let event_type = match event_val {
    Value::Object(ev) => {
      scope.push_root(Value::Object(ev))?;
      let type_key = alloc_key(scope, "type")?;
      let t = vm.get_with_host_and_hooks(host, scope, hooks, ev, type_key)?;
      to_rust_string_limited(
        scope.heap_mut(),
        t,
        FILE_READER_EVENT_TYPE_MAX_BYTES,
        "FileReader event type exceeds maximum length",
      )?
    }
    other => to_rust_string_limited(
      scope.heap_mut(),
      other,
      FILE_READER_EVENT_TYPE_MAX_BYTES,
      "FileReader event type exceeds maximum length",
    )?,
  };

  dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, &event_type)?;
  Ok(Value::Bool(true))
}

fn file_reader_microtask_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(CALLBACK_REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "FileReader microtask missing realm id slot",
    ))?;

  let reader_obj = match slots
    .get(CALLBACK_READER_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "FileReader microtask missing reader slot",
      ));
    }
  };

  let seq = slots
    .get(CALLBACK_SEQ_SLOT)
    .copied()
    .and_then(slot_number_to_u64)
    .ok_or(VmError::InvariantViolation(
      "FileReader microtask missing seq slot",
    ))?;
  let action = slots
    .get(CALLBACK_ACTION_SLOT)
    .copied()
    .and_then(slot_number_to_u8)
    .ok_or(VmError::InvariantViolation(
      "FileReader microtask missing action slot",
    ))?;

  // Check staleness (abort/new read can invalidate scheduled microtasks).
  let pending = {
    let mut registry = registry()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(realm_state) = registry.realms.get_mut(&realm_id) else {
      return Ok(Value::Undefined);
    };

    let gc_runs = scope.heap().gc_runs();
    if gc_runs != realm_state.last_gc_runs {
      realm_state.last_gc_runs = gc_runs;
      let heap = scope.heap();
      realm_state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }

    let Some(reader) = realm_state.readers.get_mut(&WeakGcObject::from(reader_obj)) else {
      return Ok(Value::Undefined);
    };
    if reader.read_seq != seq {
      return Ok(Value::Undefined);
    }

    match action {
      ACTION_READ => reader.pending.take(),
      ACTION_ABORT => None,
      _ => return Ok(Value::Undefined),
    }
  };

  scope.push_root(Value::Object(reader_obj))?;

  match action {
    ACTION_READ => {
      let Some(pending) = pending else {
        return Ok(Value::Undefined);
      };

      dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, "loadstart")?;

      // Abort/new-read can happen inside `loadstart`.
      let still_valid = {
        let mut registry = registry()
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(realm_state) = registry.realms.get_mut(&realm_id) else {
          return Ok(Value::Undefined);
        };
        let Some(reader) = realm_state.readers.get(&WeakGcObject::from(reader_obj)) else {
          return Ok(Value::Undefined);
        };
        reader.read_seq == seq && reader.ready_state == FILE_READER_LOADING
      };

      if !still_valid {
        return Ok(Value::Undefined);
      }

      let result = match pending {
        PendingRead::Text { bytes } => FileReaderResult::Text(String::from_utf8_lossy(&bytes).into_owned()),
        PendingRead::ArrayBuffer { bytes } => FileReaderResult::ArrayBuffer(bytes),
        PendingRead::DataUrl { bytes, mime } => {
          let mut out = String::with_capacity(
            "data:".len()
              .saturating_add(mime.len())
              .saturating_add(";base64,".len())
              .saturating_add(((bytes.len() + 2) / 3) * 4),
          );
          out.push_str("data:");
          out.push_str(&mime);
          out.push_str(";base64,");
          general_purpose::STANDARD.encode_string(bytes, &mut out);
          FileReaderResult::DataUrl(out)
        }
      };

      {
        let mut registry = registry()
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(realm_state) = registry.realms.get_mut(&realm_id) else {
          return Ok(Value::Undefined);
        };
        let Some(reader) = realm_state.readers.get_mut(&WeakGcObject::from(reader_obj)) else {
          return Ok(Value::Undefined);
        };
        if reader.read_seq != seq {
          return Ok(Value::Undefined);
        }
        reader.ready_state = FILE_READER_DONE;
        reader.result = result;
        reader.error = None;
      }

      dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, "load")?;
      dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, "loadend")?;
    }
    ACTION_ABORT => {
      dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, "abort")?;
      dispatch_file_reader_event(vm, scope, host, hooks, reader_obj, "loadend")?;
    }
    _ => {}
  }

  Ok(Value::Undefined)
}

pub fn install_window_file_reader_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let microtask_call_id = vm.register_native_call(file_reader_microtask_native)?;

  let call_id = vm.register_native_call(file_reader_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(file_reader_ctor_construct)?;
  let name = scope.alloc_string("FileReader")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name,
    0,
    &[Value::Number(realm_id.to_raw() as f64), Value::Object(global)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(intr.function_prototype()))?;

  let proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "FileReader constructor missing prototype object",
        ));
      }
    }
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(intr.object_prototype()))?;

  // Static constants.
  let empty_key = alloc_key(&mut scope, "EMPTY")?;
  scope.define_property(
    ctor,
    empty_key,
    read_only_data_desc(Value::Number(FILE_READER_EMPTY as f64)),
  )?;
  let loading_key = alloc_key(&mut scope, "LOADING")?;
  scope.define_property(
    ctor,
    loading_key,
    read_only_data_desc(Value::Number(FILE_READER_LOADING as f64)),
  )?;
  let done_key = alloc_key(&mut scope, "DONE")?;
  scope.define_property(
    ctor,
    done_key,
    read_only_data_desc(Value::Number(FILE_READER_DONE as f64)),
  )?;

  let slots = [Value::Number(realm_id.to_raw() as f64), Value::Object(global)];

  // Methods.
  let read_text_id = vm.register_native_call(file_reader_read_as_text_native)?;
  let read_text_name = scope.alloc_string("readAsText")?;
  scope.push_root(Value::String(read_text_name))?;
  let read_text_fn =
    scope.alloc_native_function_with_slots(read_text_id, None, read_text_name, 1, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(read_text_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(read_text_fn))?;
  let read_text_key = alloc_key(&mut scope, "readAsText")?;
  scope.define_property(proto, read_text_key, data_desc(Value::Object(read_text_fn), true))?;

  let read_ab_id = vm.register_native_call(file_reader_read_as_array_buffer_native)?;
  let read_ab_name = scope.alloc_string("readAsArrayBuffer")?;
  scope.push_root(Value::String(read_ab_name))?;
  let read_ab_fn = scope.alloc_native_function_with_slots(read_ab_id, None, read_ab_name, 1, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(read_ab_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(read_ab_fn))?;
  let read_ab_key = alloc_key(&mut scope, "readAsArrayBuffer")?;
  scope.define_property(proto, read_ab_key, data_desc(Value::Object(read_ab_fn), true))?;

  let read_du_id = vm.register_native_call(file_reader_read_as_data_url_native)?;
  let read_du_name = scope.alloc_string("readAsDataURL")?;
  scope.push_root(Value::String(read_du_name))?;
  let read_du_fn = scope.alloc_native_function_with_slots(read_du_id, None, read_du_name, 1, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(read_du_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(read_du_fn))?;
  let read_du_key = alloc_key(&mut scope, "readAsDataURL")?;
  scope.define_property(proto, read_du_key, data_desc(Value::Object(read_du_fn), true))?;

  let abort_id = vm.register_native_call(file_reader_abort_native)?;
  let abort_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(abort_name))?;
  let abort_fn = scope.alloc_native_function_with_slots(abort_id, None, abort_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(abort_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(abort_fn))?;
  let abort_key = alloc_key(&mut scope, "abort")?;
  scope.define_property(proto, abort_key, data_desc(Value::Object(abort_fn), true))?;

  let ael_id = vm.register_native_call(file_reader_add_event_listener_native)?;
  let ael_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(ael_name))?;
  let ael_fn = scope.alloc_native_function_with_slots(ael_id, None, ael_name, 2, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(ael_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(ael_fn))?;
  let ael_key = alloc_key(&mut scope, "addEventListener")?;
  scope.define_property(proto, ael_key, data_desc(Value::Object(ael_fn), true))?;

  let rel_id = vm.register_native_call(file_reader_remove_event_listener_native)?;
  let rel_name = scope.alloc_string("removeEventListener")?;
  scope.push_root(Value::String(rel_name))?;
  let rel_fn = scope.alloc_native_function_with_slots(rel_id, None, rel_name, 2, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(rel_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(rel_fn))?;
  let rel_key = alloc_key(&mut scope, "removeEventListener")?;
  scope.define_property(proto, rel_key, data_desc(Value::Object(rel_fn), true))?;

  let dispatch_id = vm.register_native_call(file_reader_dispatch_event_native)?;
  let dispatch_name = scope.alloc_string("dispatchEvent")?;
  scope.push_root(Value::String(dispatch_name))?;
  let dispatch_fn = scope.alloc_native_function_with_slots(dispatch_id, None, dispatch_name, 1, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(dispatch_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(dispatch_fn))?;
  let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
  scope.define_property(proto, dispatch_key, data_desc(Value::Object(dispatch_fn), true))?;

  // Event handler properties.
  set_data_prop(&mut scope, proto, "onloadstart", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onprogress", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onload", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onerror", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onabort", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onloadend", Value::Null, true)?;

  // Accessors.
  let func_proto = intr.function_prototype();
  let ready_state_get_id = vm.register_native_call(file_reader_ready_state_get)?;
  let ready_state_get_name = scope.alloc_string("get readyState")?;
  scope.push_root(Value::String(ready_state_get_name))?;
  let ready_state_get_fn =
    scope.alloc_native_function_with_slots(ready_state_get_id, None, ready_state_get_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(ready_state_get_fn, Some(func_proto))?;
  scope.push_root(Value::Object(ready_state_get_fn))?;
  let ready_state_key = alloc_key(&mut scope, "readyState")?;
  scope.define_property(
    proto,
    ready_state_key,
    accessor_desc(Value::Object(ready_state_get_fn), Value::Undefined),
  )?;

  let result_get_id = vm.register_native_call(file_reader_result_get)?;
  let result_get_name = scope.alloc_string("get result")?;
  scope.push_root(Value::String(result_get_name))?;
  let result_get_fn =
    scope.alloc_native_function_with_slots(result_get_id, None, result_get_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(result_get_fn, Some(func_proto))?;
  scope.push_root(Value::Object(result_get_fn))?;
  let result_key = alloc_key(&mut scope, "result")?;
  scope.define_property(
    proto,
    result_key,
    accessor_desc(Value::Object(result_get_fn), Value::Undefined),
  )?;

  let error_get_id = vm.register_native_call(file_reader_error_get)?;
  let error_get_name = scope.alloc_string("get error")?;
  scope.push_root(Value::String(error_get_name))?;
  let error_get_fn =
    scope.alloc_native_function_with_slots(error_get_id, None, error_get_name, 0, &slots)?;
  scope
    .heap_mut()
    .object_set_prototype(error_get_fn, Some(func_proto))?;
  scope.push_root(Value::Object(error_get_fn))?;
  let error_key = alloc_key(&mut scope, "error")?;
  scope.define_property(
    proto,
    error_key,
    accessor_desc(Value::Object(error_get_fn), Value::Undefined),
  )?;

  // Symbol.toStringTag.
  let to_string_tag = intr.well_known_symbols().to_string_tag;
  let tag_key = PropertyKey::from_symbol(to_string_tag);
  let tag_value = scope.alloc_string("FileReader")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(proto, tag_key, data_desc(Value::String(tag_value), false))?;

  // Expose global constructor.
  let ctor_key = alloc_key(&mut scope, "FileReader")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  registry.realms.insert(
    realm_id,
    FileReaderRealmState {
      file_reader_proto: proto,
      readers: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
      microtask_call_id,
    },
  );

  Ok(())
}

pub fn teardown_window_file_reader_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  registry.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig, WindowRealmHost};
  use crate::js::window_timers::{install_window_timers_bindings, vm_error_to_event_loop_error, VmJsEventLoopHooks};
  use vm_js::PropertyKey;

  struct Host {
    host_ctx: (),
    window: WindowRealm,
  }

  impl Host {
    fn new() -> Self {
      let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
      {
        let (vm, realm, heap) = window.vm_realm_and_heap_mut();
        install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      }
      Self { host_ctx: (), window }
    }
  }

  impl WindowRealmHost for Host {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
      (&mut self.host_ctx, &mut self.window)
    }
  }

  fn get_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
  }

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string");
    };
    heap.get_string(s).unwrap().to_utf8_lossy().to_string()
  }

  fn read_log(vm: &mut Vm, scope: &mut Scope<'_>, arr: GcObject) -> Vec<String> {
    let len_key = alloc_key(scope, "length").unwrap();
    let len_val = vm.get(scope, arr, len_key).unwrap();
    let Value::Number(n) = len_val else {
      panic!("expected length number");
    };
    let len = n as usize;
    let mut out = Vec::new();
    for idx in 0..len {
      let k = alloc_key(scope, &idx.to_string()).unwrap();
      let v = vm.get(scope, arr, k).unwrap();
      out.push(get_string(scope.heap(), v));
    }
    out
  }

  #[test]
  fn file_reader_read_as_text_fires_events_and_sets_result() -> crate::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log=[];\n\
         globalThis.__result=null;\n\
         const r = new FileReader();\n\
         r.onloadstart = function(){ __log.push('loadstart'); };\n\
         r.onload = function(){ __log.push('load'); };\n\
         r.onloadend = function(){ __log.push('loadend'); __result = r.result; };\n\
         r.readAsText(new Blob(['hi'], {type:'text/plain'}));",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();

    let log_arr = match get_prop(&mut scope, global, "__log") {
      Value::Object(obj) => obj,
      _ => panic!("expected __log array"),
    };
    let log = read_log(vm, &mut scope, log_arr);
    assert_eq!(log, vec!["loadstart", "load", "loadend"]);
    let result_val = get_prop(&mut scope, global, "__result");
    assert_eq!(get_string(scope.heap(), result_val), "hi");
    Ok(())
  }

  #[test]
  fn file_reader_read_as_array_buffer_sets_result() -> crate::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log=[];\n\
         globalThis.__result=null;\n\
         const r = new FileReader();\n\
         r.onloadend = function(){ __log.push('loadend'); __result = r.result; };\n\
         r.readAsArrayBuffer(new Blob(['hi']));",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();

    let result = get_prop(&mut scope, global, "__result");
    let Value::Object(ab) = result else {
      panic!("expected ArrayBuffer result");
    };
    assert!(scope.heap().is_array_buffer_object(ab));
    assert_eq!(scope.heap().array_buffer_data(ab).unwrap(), b"hi");
    Ok(())
  }

  #[test]
  fn file_reader_read_as_data_url_sets_result() -> crate::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__result=null;\n\
         const r = new FileReader();\n\
         r.onloadend = function(){ __result = r.result; };\n\
         r.readAsDataURL(new Blob(['hi'], {type:'text/plain'}));",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    let result_val = get_prop(&mut scope, global, "__result");
    let out = get_string(scope.heap(), result_val);
    assert_eq!(out, "data:text/plain;base64,aGk=");
    Ok(())
  }

  #[test]
  fn file_reader_abort_skips_load_and_fires_abort_and_loadend() -> crate::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log=[];\n\
         const r = new FileReader();\n\
         r.onload = function(){ __log.push('load'); };\n\
         r.onabort = function(){ __log.push('abort'); };\n\
         r.onloadend = function(){ __log.push('loadend'); };\n\
         r.readAsText(new Blob(['hi']));\n\
         r.abort();",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();

    let log_arr = match get_prop(&mut scope, global, "__log") {
      Value::Object(obj) => obj,
      _ => panic!("expected __log array"),
    };
    let log = read_log(vm, &mut scope, log_arr);
    assert_eq!(log, vec!["abort", "loadend"]);
    Ok(())
  }
}
