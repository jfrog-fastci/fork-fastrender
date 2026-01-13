//! Minimal `FileReader` implementation for `vm-js` Window realms.
//!
//! This is a spec-shaped, practical MVP intended to unblock real-world scripts that expect
//! `FileReader` to exist and to behave asynchronously (callbacks + events).
//!
//! FastRender's `vm-js` embedding is single-threaded and has no OS-backed file I/O. This
//! implementation therefore only supports reading in-memory `Blob` data (from `window_blob`).
//!
//! Reads are scheduled onto the host [`crate::js::event_loop::EventLoop`] and complete in a single
//! task.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose;
use base64::Engine as _;
use vm_js::{
  GcObject, Heap, HostSlots, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

use crate::js::event_loop::TaskSource;
use crate::js::window_blob::{self, BlobData};
use crate::js::window_realm::{WindowRealmHost, EVENT_TARGET_HOST_TAG};
use crate::js::window_timers::{event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks};

const REALM_ID_SLOT: usize = 0;

const FILE_READER_HOST_TAG: u64 = 0x4649_4C45_5245_4144; // "FILEREAD"

const READY_STATE_EMPTY: u8 = 0;
const READY_STATE_LOADING: u8 = 1;
const READY_STATE_DONE: u8 = 2;

/// Hard cap on `readAsDataURL()` output size (in UTF-8 bytes).
///
/// `Blob` itself is bounded to 10MiB in `window_blob`, so this is primarily a guardrail against
/// future changes and accidental large string allocations.
const MAX_DATA_URL_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum ReadKind {
  ArrayBuffer,
  Text,
  DataUrl,
  BinaryString,
}

#[derive(Debug, Clone)]
struct FileReaderState {
  ready_state: u8,
  /// Monotonic per-instance operation id used to ignore stale tasks.
  active_op: u64,
  aborted: bool,
}

impl Default for FileReaderState {
  fn default() -> Self {
    Self {
      ready_state: READY_STATE_EMPTY,
      active_op: 0,
      aborted: false,
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
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

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut FileReaderRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "FileReader bindings used before install_window_file_reader_bindings",
    ))?;

  // Opportunistically sweep dead FileReaders when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn get_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_own_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn set_brand(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<(), VmError> {
  let key = alloc_key(scope, name)?;
  scope.define_property(
    obj,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Bool(true),
        writable: false,
      },
    },
  )
}

fn create_dom_exception_like(scope: &mut Scope<'_>, name: &str, message: &str) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;

  let name_key = alloc_key(scope, "name")?;
  let message_key = alloc_key(scope, "message")?;

  scope.define_property(
    obj,
    name_key,
    data_desc(Value::String(name_s), /* writable */ false),
  )?;
  scope.define_property(
    obj,
    message_key,
    data_desc(Value::String(message_s), /* writable */ false),
  )?;

  Ok(Value::Object(obj))
}

fn create_abort_error(scope: &mut Scope<'_>) -> Result<Value, VmError> {
  // Keep this aligned with `window_abort`'s default abort reason.
  create_dom_exception_like(scope, "AbortError", "This operation was aborted")
}

fn create_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global: GcObject,
  event_type: &str,
) -> Result<Value, VmError> {
  // `EventTarget.dispatchEvent` requires branded `Event` objects.
  //
  // We construct the realm's global `Event` rather than trying to replicate the internal branding
  // state here.
  scope.push_root(Value::Object(global))?;
  let event_ctor = {
    let key_s = scope.alloc_string("Event")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    vm.get_with_host_and_hooks(host, scope, hooks, global, key)?
  };
  let Value::Object(event_ctor_obj) = event_ctor else {
    return Err(VmError::TypeError("Event is not available"));
  };
  scope.push_root(Value::Object(event_ctor_obj))?;

  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;

  let event = vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(event_ctor_obj),
    &[Value::String(type_s)],
    Value::Object(event_ctor_obj),
  )?;
  let Value::Object(_) = event else {
    return Err(VmError::InvariantViolation("Event constructor must return an object"));
  };
  Ok(event)
}

fn create_progress_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global: GcObject,
  loaded: usize,
  total: usize,
) -> Result<Value, VmError> {
  let event = create_event(vm, scope, host, hooks, global, "progress")?;
  let Value::Object(event_obj) = event else {
    return Err(VmError::InvariantViolation(
      "Event constructor must return an object",
    ));
  };
  scope.push_root(Value::Object(event_obj))?;

  let loaded_key = alloc_key(scope, "loaded")?;
  scope.define_property(
    event_obj,
    loaded_key,
    data_desc(Value::Number(loaded as f64), /* writable */ false),
  )?;

  let total_key = alloc_key(scope, "total")?;
  scope.define_property(
    event_obj,
    total_key,
    data_desc(Value::Number(total as f64), /* writable */ false),
  )?;

  let length_key = alloc_key(scope, "lengthComputable")?;
  scope.define_property(
    event_obj,
    length_key,
    data_desc(Value::Bool(true), /* writable */ false),
  )?;

  Ok(Value::Object(event_obj))
}

fn dispatch_event_and_handler(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  event: Value,
  handler_prop: &str,
) -> Result<(), VmError> {
  scope.push_root(event)?;

  let dispatch_fn = {
    let key = alloc_key(scope, "dispatchEvent")?;
    vm.get_with_host_and_hooks(host, scope, hooks, target, key)?
  };
  if scope.heap().is_callable(dispatch_fn).unwrap_or(false) {
    if let Err(err) = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      dispatch_fn,
      Value::Object(target),
      &[event],
    ) {
      if matches!(err, VmError::Termination(_)) {
        return Err(err);
      }
    }
  }

  let handler = get_own_data_prop(scope, target, handler_prop)?;
  if scope.heap().is_callable(handler).unwrap_or(false) {
    if let Err(err) = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      handler,
      Value::Object(target),
      &[event],
    ) {
      if matches!(err, VmError::Termination(_)) {
        return Err(err);
      }
    }
  }

  Ok(())
}

fn require_file_reader(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<(GcObject, FileReaderState), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("FileReader: illegal invocation"));
  };

  // Fast pre-check: reject obviously wrong receivers without taking the global registry lock.
  match scope.heap().object_host_slots(obj)? {
    Some(slots) if slots.a == FILE_READER_HOST_TAG => {}
    _ => return Err(VmError::TypeError("FileReader: illegal invocation")),
  };

  let state = with_realm_state_mut(vm, scope, callee, |realm_state| {
    realm_state
      .readers
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))
  })?;
  Ok((obj, state))
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
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let reader = scope.alloc_object()?;
  scope.push_root(Value::Object(reader))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(reader, Some(proto))?;
  }
  // Brand the object as a `FileReader` (host slots `a`) and an `EventTarget` (host slots `b`).
  scope.heap_mut().object_set_host_slots(
    reader,
    HostSlots {
      a: FILE_READER_HOST_TAG,
      b: EVENT_TARGET_HOST_TAG,
    },
  )?;

  // Public instance properties.
  set_own_data_prop(
    scope,
    reader,
    "readyState",
    Value::Number(READY_STATE_EMPTY as f64),
    /* writable */ false,
  )?;
  set_own_data_prop(scope, reader, "result", Value::Null, /* writable */ false)?;
  set_own_data_prop(scope, reader, "error", Value::Null, /* writable */ false)?;

  for prop in [
    "onloadstart",
    "onprogress",
    "onload",
    "onerror",
    "onabort",
    "onloadend",
  ] {
    set_own_data_prop(scope, reader, prop, Value::Null, /* writable */ true)?;
  }

  with_realm_state_mut(vm, scope, callee, |realm_state| {
    realm_state
      .readers
      .insert(WeakGcObject::from(reader), FileReaderState::default());
    Ok(())
  })?;

  Ok(Value::Object(reader))
}

fn start_read_operation(
  realm_id: RealmId,
  heap: &Heap,
  reader_obj: GcObject,
  op_id: u64,
) -> Result<Option<FileReaderState>, VmError> {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let Some(realm_state) = registry.realms.get_mut(&realm_id) else {
    return Ok(None);
  };

  // Opportunistic sweep.
  let gc_runs = heap.gc_runs();
  if gc_runs != realm_state.last_gc_runs {
    realm_state.last_gc_runs = gc_runs;
    realm_state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  let state = realm_state.readers.get(&WeakGcObject::from(reader_obj)).cloned();
  Ok(state.filter(|s| s.ready_state == READY_STATE_LOADING && s.active_op == op_id && !s.aborted))
}

fn file_reader_read_common<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
  kind: ReadKind,
) -> Result<Value, VmError> {
  let (reader_obj, state) = require_file_reader(vm, scope, callee, this)?;
  if state.ready_state == READY_STATE_LOADING {
    return Err(VmError::TypeError("FileReader is already loading"));
  }

  let blob_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), blob_value)? else {
    return Err(VmError::TypeError("FileReader: argument is not a Blob"));
  };
  let BlobData { bytes, r#type } = blob;

  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  // Update internal state before queueing the task so abort() and state checks can observe LOADING.
  let op_id = with_realm_state_mut(vm, scope, callee, |realm_state| {
    let entry = realm_state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))?;
    if entry.ready_state == READY_STATE_LOADING {
      return Err(VmError::TypeError("FileReader is already loading"));
    }
    entry.ready_state = READY_STATE_LOADING;
    entry.aborted = false;
    entry.active_op = entry.active_op.saturating_add(1);
    Ok(entry.active_op)
  })?;

  // Public observable state.
  set_own_data_prop(
    scope,
    reader_obj,
    "readyState",
    Value::Number(READY_STATE_LOADING as f64),
    /* writable */ false,
  )?;
  set_own_data_prop(scope, reader_obj, "result", Value::Null, /* writable */ false)?;
  set_own_data_prop(scope, reader_obj, "error", Value::Null, /* writable */ false)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    // Revert state so the instance doesn't get stuck in LOADING.
    let _ = with_realm_state_mut(vm, scope, callee, |realm_state| {
      if let Some(entry) = realm_state.readers.get_mut(&WeakGcObject::from(reader_obj)) {
        entry.ready_state = READY_STATE_EMPTY;
        entry.aborted = false;
      }
      Ok(())
    });
    let _ = set_own_data_prop(
      scope,
      reader_obj,
      "readyState",
      Value::Number(READY_STATE_EMPTY as f64),
      /* writable */ false,
    );
    return Err(VmError::TypeError("FileReader requires an active EventLoop"));
  };

  let root: RootId = scope.heap_mut().add_root(Value::Object(reader_obj))?;

  let queue_result = event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    hooks.set_event_loop(event_loop);
    let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
    window_realm.reset_interrupt();
    let budget = window_realm.vm_budget_now();
    let global = window_realm.global_object();
    let (vm, heap) = window_realm.vm_and_heap_mut();
    let mut vm = vm.push_budget(budget);
    let tick_result = vm.tick();

    let call_result: Result<(), VmError> = match tick_result {
      Ok(()) => {
        let reader_value = heap.get_root(root).unwrap_or(Value::Undefined);
        let result = (|| {
          let Value::Object(reader_obj) = reader_value else {
            return Ok(());
          };

        // Ensure the op is still current before running any JS.
        if start_read_operation(realm_id, heap, reader_obj, op_id)?.is_none() {
          return Ok(());
        }

        let mut scope = heap.scope();
        scope.push_root(Value::Object(reader_obj))?;

        // loadstart (before result).
        let ev = create_event(&mut vm, &mut scope, vm_host, &mut hooks, global, "loadstart")?;
        dispatch_event_and_handler(&mut vm, &mut scope, vm_host, &mut hooks, reader_obj, ev, "onloadstart")?;
        // User code can call abort() during loadstart.
        if start_read_operation(realm_id, scope.heap(), reader_obj, op_id)?.is_none() {
          return Ok(());
        }

        // progress (single-shot).
        let progress_ev = create_progress_event(
          &mut vm,
          &mut scope,
          vm_host,
          &mut hooks,
          global,
          bytes.len(),
          bytes.len(),
        )?;
        dispatch_event_and_handler(
          &mut vm,
          &mut scope,
          vm_host,
          &mut hooks,
          reader_obj,
          progress_ev,
          "onprogress",
        )?;
        if start_read_operation(realm_id, scope.heap(), reader_obj, op_id)?.is_none() {
          return Ok(());
        }

        // Produce result / error.
        let (result_value, error_value, outcome_event, outcome_handler) = match kind {
          ReadKind::ArrayBuffer => {
            let intr = vm
              .intrinsics()
              .ok_or(VmError::Unimplemented("FileReader requires intrinsics"))?;
            let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
            scope.push_root(Value::Object(ab))?;
            scope
              .heap_mut()
              .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
            (Value::Object(ab), Value::Null, "load", "onload")
          }
          ReadKind::Text => {
            let text = String::from_utf8_lossy(&bytes);
            let s = scope.alloc_string(&text)?;
            (Value::String(s), Value::Null, "load", "onload")
          }
          ReadKind::BinaryString => {
            let mut out = String::new();
            out
              .try_reserve_exact(bytes.len())
              .map_err(|_| VmError::OutOfMemory)?;
            for b in bytes {
              out.push(char::from(b));
            }
            let s = scope.alloc_string(&out)?;
            (Value::String(s), Value::Null, "load", "onload")
          }
          ReadKind::DataUrl => {
            let mime = if r#type.is_empty() {
              "application/octet-stream"
            } else {
              r#type.as_str()
            };

            let base64_len = ((bytes.len() + 2) / 3)
              .checked_mul(4)
              .ok_or(VmError::OutOfMemory)?;
            let total_len = 5usize
              .checked_add(mime.len())
              .and_then(|n| n.checked_add(8))
              .and_then(|n| n.checked_add(base64_len))
              .ok_or(VmError::OutOfMemory)?;
            if total_len > MAX_DATA_URL_BYTES {
              let err = create_dom_exception_like(
                &mut scope,
                "NotReadableError",
                "FileReader result exceeds maximum length",
              )?;
              (Value::Null, err, "error", "onerror")
            } else {
              let b64 = general_purpose::STANDARD.encode(&bytes);
              let mut data_url = String::with_capacity(total_len);
              data_url.push_str("data:");
              data_url.push_str(mime);
              data_url.push_str(";base64,");
              data_url.push_str(&b64);
              let s = scope.alloc_string(&data_url)?;
              (Value::String(s), Value::Null, "load", "onload")
            }
          }
        };

        // Transition to DONE and publish result/error.
        set_own_data_prop(
          &mut scope,
          reader_obj,
          "readyState",
          Value::Number(READY_STATE_DONE as f64),
          /* writable */ false,
        )?;
        set_own_data_prop(&mut scope, reader_obj, "result", result_value, /* writable */ false)?;
        set_own_data_prop(&mut scope, reader_obj, "error", error_value, /* writable */ false)?;

        // Update internal state.
        {
          let mut reg = registry()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
          if let Some(realm_state) = reg.realms.get_mut(&realm_id) {
            if let Some(entry) = realm_state.readers.get_mut(&WeakGcObject::from(reader_obj)) {
              if entry.active_op == op_id {
                entry.ready_state = READY_STATE_DONE;
              }
            }
          }
        }

        // Outcome event (load/error).
        let outcome_ev = create_event(&mut vm, &mut scope, vm_host, &mut hooks, global, outcome_event)?;
        dispatch_event_and_handler(
          &mut vm,
          &mut scope,
          vm_host,
          &mut hooks,
          reader_obj,
          outcome_ev,
          outcome_handler,
        )?;

        // loadend.
        let end_ev = create_event(&mut vm, &mut scope, vm_host, &mut hooks, global, "loadend")?;
        dispatch_event_and_handler(&mut vm, &mut scope, vm_host, &mut hooks, reader_obj, end_ev, "onloadend")?;

        Ok(())
        })();

        heap.remove_root(root);
        result
      }
      Err(err) => {
        heap.remove_root(root);
        Err(err)
      }
    };

    let finish_err = hooks.finish(heap);
    if let Some(err) = finish_err {
      return Err(err);
    }

    call_result
      .map_err(|err| vm_error_to_event_loop_error(heap, err))
      .map(|_| ())
  });

  if let Err(e) = queue_result {
    scope.heap_mut().remove_root(root);
    // Revert state so the instance doesn't get stuck in LOADING.
    with_realm_state_mut(vm, scope, callee, |realm_state| {
      if let Some(entry) = realm_state.readers.get_mut(&WeakGcObject::from(reader_obj)) {
        entry.ready_state = READY_STATE_EMPTY;
        entry.aborted = false;
      }
      Ok(())
    })?;
    set_own_data_prop(
      scope,
      reader_obj,
      "readyState",
      Value::Number(READY_STATE_EMPTY as f64),
      /* writable */ false,
    )?;
    return Err(throw_error(scope, &format!("{e}")));
  }

  Ok(Value::Undefined)
}

fn file_reader_read_as_array_buffer_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  file_reader_read_common::<Host>(vm, scope, host, hooks, callee, this, args, ReadKind::ArrayBuffer)
}

fn file_reader_read_as_text_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  file_reader_read_common::<Host>(vm, scope, host, hooks, callee, this, args, ReadKind::Text)
}

fn file_reader_read_as_data_url_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  file_reader_read_common::<Host>(vm, scope, host, hooks, callee, this, args, ReadKind::DataUrl)
}

fn file_reader_read_as_binary_string_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  file_reader_read_common::<Host>(vm, scope, host, hooks, callee, this, args, ReadKind::BinaryString)
}

fn file_reader_abort_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (reader_obj, state) = require_file_reader(vm, scope, callee, this)?;
  if state.ready_state != READY_STATE_LOADING {
    return Ok(Value::Undefined);
  }

  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let abort_error = create_abort_error(scope)?;
  scope.push_root(abort_error)?;

  // Update state synchronously so any queued read task can observe the cancellation.
  let op_id = with_realm_state_mut(vm, scope, callee, |realm_state| {
    let entry = realm_state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError("FileReader: illegal invocation"))?;
    if entry.ready_state != READY_STATE_LOADING {
      return Ok(None);
    }
    entry.ready_state = READY_STATE_DONE;
    entry.aborted = true;
    Ok(Some(entry.active_op))
  })?;

  let Some(op_id) = op_id else {
    return Ok(Value::Undefined);
  };

  set_own_data_prop(
    scope,
    reader_obj,
    "readyState",
    Value::Number(READY_STATE_DONE as f64),
    /* writable */ false,
  )?;
  set_own_data_prop(scope, reader_obj, "result", Value::Null, /* writable */ false)?;
  set_own_data_prop(scope, reader_obj, "error", abort_error, /* writable */ false)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(VmError::TypeError("FileReader requires an active EventLoop"));
  };

  let root: RootId = scope.heap_mut().add_root(Value::Object(reader_obj))?;

  let queue_result = event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    hooks.set_event_loop(event_loop);
    let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
    window_realm.reset_interrupt();
    let budget = window_realm.vm_budget_now();
    let global = window_realm.global_object();
    let (vm, heap) = window_realm.vm_and_heap_mut();
    let mut vm = vm.push_budget(budget);
    let tick_result = vm.tick();

    let call_result: Result<(), VmError> = match tick_result {
      Ok(()) => {
        let reader_value = heap.get_root(root).unwrap_or(Value::Undefined);
        let result = (|| {
          let Value::Object(reader_obj) = reader_value else {
            return Ok(());
          };

        // Only dispatch abort for the op we canceled.
        let should_dispatch = {
          let mut reg = registry()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
          reg
            .realms
            .get_mut(&realm_id)
            .and_then(|realm_state| realm_state.readers.get(&WeakGcObject::from(reader_obj)).cloned())
            .is_some_and(|s| s.active_op == op_id && s.aborted)
        };
        if !should_dispatch {
          return Ok(());
        }

        let mut scope = heap.scope();
        scope.push_root(Value::Object(reader_obj))?;

        let abort_ev = create_event(&mut vm, &mut scope, vm_host, &mut hooks, global, "abort")?;
        dispatch_event_and_handler(&mut vm, &mut scope, vm_host, &mut hooks, reader_obj, abort_ev, "onabort")?;

        let end_ev = create_event(&mut vm, &mut scope, vm_host, &mut hooks, global, "loadend")?;
        dispatch_event_and_handler(&mut vm, &mut scope, vm_host, &mut hooks, reader_obj, end_ev, "onloadend")?;

        Ok(())
        })();

        heap.remove_root(root);
        result
      }
      Err(err) => {
        heap.remove_root(root);
        Err(err)
      }
    };

    let finish_err = hooks.finish(heap);
    if let Some(err) = finish_err {
      return Err(err);
    }

    call_result
      .map_err(|err| vm_error_to_event_loop_error(heap, err))
      .map(|_| ())
  });

  if let Err(e) = queue_result {
    scope.heap_mut().remove_root(root);
    return Err(throw_error(scope, &format!("{e}")));
  }

  Ok(Value::Undefined)
}

/// Install `FileReader` onto the global object of a `vm-js` Window realm.
pub fn install_window_file_reader_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();
  let global = realm.global_object();
  let realm_id = realm.id();

  let mut scope = heap.scope();
  scope.push_root(Value::Object(global))?;

  // Look up the existing `EventTarget.prototype` installed by `WindowRealm`.
  let event_target_proto = {
    let event_target_key = alloc_key(&mut scope, "EventTarget")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &event_target_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented(
        "EventTarget is not installed on the global object",
      ))?;

    let proto_key = alloc_key(&mut scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &proto_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented("EventTarget.prototype is missing"))?
  };

  // --- FileReader constructor ------------------------------------------------
  let call_id: NativeFunctionId = vm.register_native_call(file_reader_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(file_reader_ctor_construct)?;
  let name_s = scope.alloc_string("FileReader")?;
  scope.push_root(Value::String(name_s))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name_s,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;

  // Static constants.
  set_own_data_prop(
    &mut scope,
    ctor,
    "EMPTY",
    Value::Number(READY_STATE_EMPTY as f64),
    /* writable */ false,
  )?;
  set_own_data_prop(
    &mut scope,
    ctor,
    "LOADING",
    Value::Number(READY_STATE_LOADING as f64),
    /* writable */ false,
  )?;
  set_own_data_prop(
    &mut scope,
    ctor,
    "DONE",
    Value::Number(READY_STATE_DONE as f64),
    /* writable */ false,
  )?;

  // Prototype object created by vm-js; install methods.
  let Value::Object(proto) = get_own_data_prop(&mut scope, ctor, "prototype")? else {
    return Err(VmError::InvariantViolation("FileReader.prototype missing"));
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(event_target_proto))?;

  let make_method =
    |scope: &mut Scope<'_>, name: &str, call: NativeFunctionId, argc: u32| -> Result<GcObject, VmError> {
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function_with_slots(
      call,
      None,
      name_s,
      argc,
      &[Value::Number(realm_id.to_raw() as f64)],
    )?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    Ok(func)
  };

  let read_ab_call = vm.register_native_call(file_reader_read_as_array_buffer_native::<Host>)?;
  let read_ab_fn = make_method(&mut scope, "readAsArrayBuffer", read_ab_call, 1)?;
  set_own_data_prop(
    &mut scope,
    proto,
    "readAsArrayBuffer",
    Value::Object(read_ab_fn),
    /* writable */ true,
  )?;

  let read_text_call = vm.register_native_call(file_reader_read_as_text_native::<Host>)?;
  let read_text_fn = make_method(&mut scope, "readAsText", read_text_call, 1)?;
  set_own_data_prop(
    &mut scope,
    proto,
    "readAsText",
    Value::Object(read_text_fn),
    /* writable */ true,
  )?;

  let read_data_url_call = vm.register_native_call(file_reader_read_as_data_url_native::<Host>)?;
  let read_data_url_fn = make_method(&mut scope, "readAsDataURL", read_data_url_call, 1)?;
  set_own_data_prop(
    &mut scope,
    proto,
    "readAsDataURL",
    Value::Object(read_data_url_fn),
    /* writable */ true,
  )?;

  // Optional: readAsBinaryString (some legacy libs still use it).
  let read_bin_call = vm.register_native_call(file_reader_read_as_binary_string_native::<Host>)?;
  let read_bin_fn = make_method(&mut scope, "readAsBinaryString", read_bin_call, 1)?;
  set_own_data_prop(
    &mut scope,
    proto,
    "readAsBinaryString",
    Value::Object(read_bin_fn),
    /* writable */ true,
  )?;

  let abort_call = vm.register_native_call(file_reader_abort_native::<Host>)?;
  let abort_fn = make_method(&mut scope, "abort", abort_call, 0)?;
  set_own_data_prop(&mut scope, proto, "abort", Value::Object(abort_fn), /* writable */ true)?;

  // Expose on global.
  set_own_data_prop(&mut scope, global, "FileReader", Value::Object(ctor), /* writable */ true)?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    FileReaderRealmState {
      file_reader_proto: proto,
      readers: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

/// Tear down the internal per-realm state for `FileReader`.
///
/// This is safe to call even if the bindings were never installed for the realm.
pub fn teardown_window_file_reader_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::js::window::WindowHost;
  use crate::js::RunLimits;
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> crate::error::Result<WindowHost> {
    WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
  }

  #[test]
  fn read_as_text_fires_onload_and_sets_result() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    host.exec_script(
      "globalThis.onloadCalled = false;\n\
       globalThis.textResult = null;\n\
       globalThis.reader = new FileReader();\n\
       reader.onload = () => { onloadCalled = true; textResult = reader.result; };\n\
       reader.readAsText(new Blob(['hi']));",
    )?;

    // Must be async: no events yet.
    assert_eq!(host.exec_script("onloadCalled")?, Value::Bool(false));

    let _ = host.run_until_idle(RunLimits::unbounded())?;

    assert_eq!(host.exec_script("onloadCalled")?, Value::Bool(true));
    let text = host.exec_script("textResult")?;
    let heap = host.host_mut().window_realm()?.heap();
    assert_eq!(get_string(heap, text), "hi");
    Ok(())
  }

  #[test]
  fn add_event_listener_load_works() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    host.exec_script(
      "globalThis.loadSeen = false;\n\
       globalThis.reader = new FileReader();\n\
       reader.addEventListener('load', () => { loadSeen = true; });\n\
       reader.readAsText(new Blob(['ok']));",
    )?;

    assert_eq!(host.exec_script("loadSeen")?, Value::Bool(false));
    let _ = host.run_until_idle(RunLimits::unbounded())?;
    assert_eq!(host.exec_script("loadSeen")?, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn read_as_array_buffer_matches_bytes() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    host.exec_script(
      "globalThis.buf = null;\n\
       globalThis.reader = new FileReader();\n\
       reader.onload = () => { buf = reader.result; };\n\
       reader.readAsArrayBuffer(new Blob([String.fromCharCode(1,2,3)], { type: 'application/octet-stream' }));",
    )?;

    let _ = host.run_until_idle(RunLimits::unbounded())?;
    let buf = host.exec_script("buf")?;
    let Value::Object(ab_obj) = buf else {
      panic!("expected ArrayBuffer object");
    };
    let heap = host.host_mut().window_realm()?.heap();
    assert!(heap.is_array_buffer_object(ab_obj));
    assert_eq!(heap.array_buffer_data(ab_obj).unwrap(), &[1, 2, 3]);
    Ok(())
  }

  #[test]
  fn abort_cancels_and_fires_abort_and_loadend() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    host.exec_script(
      "globalThis.events = [];\n\
       globalThis.reader = new FileReader();\n\
       reader.onabort = () => { events.push('abort'); };\n\
       reader.onloadend = () => { events.push('loadend'); };\n\
       reader.readAsText(new Blob(['hi']));\n\
       reader.abort();",
    )?;

    let _ = host.run_until_idle(RunLimits::unbounded())?;

    let events = host.exec_script("events.join(',')")?;
    {
      let heap = host.host_mut().window_realm()?.heap();
      assert_eq!(get_string(heap, events), "abort,loadend");
    }
    assert_eq!(host.exec_script("reader.result")?, Value::Null);
    Ok(())
  }

  #[test]
  fn structured_clone_rejects_file_reader() -> crate::error::Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    let ok = host.exec_script(
      "(() => {\
         try {\
           structuredClone(new FileReader());\
           return false;\
         } catch (e) {\
           return !!e && e.name === 'DataCloneError';\
         }\
       })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }
}
