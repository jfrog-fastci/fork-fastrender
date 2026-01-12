//! Minimal WHATWG `ReadableStream` (byte-oriented) implementation for `vm-js` Window realms.
//!
//! This is a deliberately small subset used by streaming consumers in Fetch/Blob-like APIs.
//! The only supported chunk type is `Uint8Array`.
//!
//! Notably absent:
//! - underlying source/controller APIs (`start`, `pull`, `enqueue`, backpressure)
//! - BYOB readers
//! - piping/teeing
//!
//! The goal is to provide just enough surface area for real-world code that expects `ReadableStream`
//! to exist and for host-owned byte sources to be consumed via `getReader().read()`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vm_js::{
  new_promise_capability_with_host_and_hooks, new_type_error_object, GcObject, Heap,
  NativeConstructId, NativeFunctionId, PromiseCapability, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const STREAM_REALM_ID_SLOT: usize = 0;

/// Maximum bytes returned by a single `reader.read()` call.
///
/// This is an internal chunking detail; it bounds per-read allocation while still being reasonably
/// large for network payloads.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamLifecycleState {
  Readable,
  Closed,
  Errored,
}

type LazyInit = Box<dyn FnOnce() -> Result<Vec<u8>, VmError> + Send + 'static>;

struct StreamState {
  locked: bool,
  state: StreamLifecycleState,
  error_message: Option<String>,
  bytes: Vec<u8>,
  init: Option<LazyInit>,
  offset: usize,
}

impl StreamState {
  fn new_from_bytes(bytes: Vec<u8>) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      error_message: None,
      bytes,
      init: None,
      offset: 0,
    }
  }

  fn new_lazy(init: LazyInit) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      error_message: None,
      bytes: Vec::new(),
      init: Some(init),
      offset: 0,
    }
  }
}

#[derive(Debug, Clone)]
struct ReaderState {
  /// `None` after `releaseLock()`.
  stream: Option<WeakGcObject>,
}

#[derive(Default)]
struct StreamRegistry {
  realms: HashMap<RealmId, StreamRealmState>,
}

struct StreamRealmState {
  readable_stream_proto: GcObject,
  reader_proto: GcObject,
  streams: HashMap<WeakGcObject, StreamState>,
  readers: HashMap<WeakGcObject, ReaderState>,
  last_gc_runs: u64,
}

static REGISTRY: OnceLock<Mutex<StreamRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<StreamRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(StreamRegistry::default()))
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn result_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
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
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "ReadableStream bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut StreamRealmState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;

  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "ReadableStream bindings used before install_window_streams_bindings",
    ))?;

  // Opportunistically sweep dead objects when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    state.streams.retain(|k, _| k.upgrade(heap).is_some());
    state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state)
}

fn readable_stream_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("ReadableStream constructor requires 'new'"))
}

fn readable_stream_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.readable_stream_proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state
      .streams
      .insert(WeakGcObject::from(obj), StreamState::new_from_bytes(Vec::new()));
    Ok(())
  })?;

  Ok(Value::Object(obj))
}

fn readable_stream_get_reader_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream_obj) = this else {
    return Err(VmError::TypeError("ReadableStream.getReader: illegal invocation"));
  };

  let reader_proto = with_realm_state_mut(vm, scope, callee, |state| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream.getReader: illegal invocation"))?;
    if stream_state.locked {
      return Err(VmError::TypeError("ReadableStream is locked"));
    }
    stream_state.locked = true;
    Ok(state.reader_proto)
  })?;

  let reader_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(reader_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(reader_obj, Some(reader_proto))?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.readers.insert(
      WeakGcObject::from(reader_obj),
      ReaderState {
        stream: Some(WeakGcObject::from(stream_obj)),
      },
    );
    Ok(())
  })?;

  Ok(Value::Object(reader_obj))
}

fn readable_stream_cancel_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream_obj) = this else {
    return Err(VmError::TypeError("ReadableStream.cancel: illegal invocation"));
  };

  // Always return a Promise (spec shape), even though we resolve synchronously.
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  // Perform the cancel synchronously, but reject if the stream is locked.
  let cancel_result = with_realm_state_mut(vm, scope, callee, |state| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream.cancel: illegal invocation"))?;
    if stream_state.locked {
      return Err(VmError::TypeError("ReadableStream is locked"));
    }

    stream_state.state = StreamLifecycleState::Closed;
    stream_state.error_message = None;
    stream_state.bytes.clear();
    stream_state.init = None;
    stream_state.offset = 0;
    Ok(())
  });

  match cancel_result {
    Ok(()) => {
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
    }
    Err(err) => {
      // `ReadableStream.cancel()` should throw if locked; preserve that behavior even though we
      // already created a Promise capability.
      return Err(err);
    }
  }

  Ok(promise)
}

fn readable_stream_locked_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream_obj) = this else {
    return Err(VmError::TypeError("ReadableStream.locked: illegal invocation"));
  };

  with_realm_state_mut(vm, scope, callee, |state| {
    let stream_state = state
      .streams
      .get(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream.locked: illegal invocation"))?;
    Ok(Value::Bool(stream_state.locked))
  })
}

fn reader_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "ReadableStreamDefaultReader constructor requires 'new'",
  ))
}

fn reader_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let stream_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(stream_obj) = stream_val else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader expects a ReadableStream",
    ));
  };

  // `new ReadableStreamDefaultReader(stream)` behaves like `stream.getReader()`.
  readable_stream_get_reader_native(vm, scope, host, hooks, callee, Value::Object(stream_obj), &[])
}

enum ReadOutcome {
  Chunk(Vec<u8>),
  Done,
  Error(String),
}

fn reader_read_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(reader_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.read: illegal invocation",
    ));
  };

  // Always return a Promise (spec shape), even though we resolve synchronously.
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("ReadableStream requires intrinsics"))?;

  let outcome = with_realm_state_mut(vm, scope, callee, |state| {
    let reader_state = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.read: illegal invocation",
      ))?;

    let Some(stream_weak) = reader_state.stream else {
      return Ok(ReadOutcome::Error(
        "ReadableStreamDefaultReader has no stream (lock released)".to_string(),
      ));
    };

    let stream_state = state
      .streams
      .get_mut(&stream_weak)
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader has an invalid stream",
      ))?;

    match stream_state.state {
      StreamLifecycleState::Readable => {}
      StreamLifecycleState::Closed => return Ok(ReadOutcome::Done),
      StreamLifecycleState::Errored => {
        let msg = stream_state
          .error_message
          .clone()
          .unwrap_or_else(|| "ReadableStream errored".to_string());
        return Ok(ReadOutcome::Error(msg));
      }
    }

    // Lazily populate bytes from the host init closure on the first `read()`.
    if let Some(init) = stream_state.init.take() {
      match init() {
        Ok(bytes) => {
          stream_state.bytes = bytes;
          stream_state.offset = 0;
        }
        Err(err) => {
          stream_state.state = StreamLifecycleState::Errored;
          let msg = err.to_string();
          stream_state.error_message = Some(msg.clone());
          return Ok(ReadOutcome::Error(msg));
        }
      }
    }

    if stream_state.offset >= stream_state.bytes.len() {
      stream_state.state = StreamLifecycleState::Closed;
      return Ok(ReadOutcome::Done);
    }

    let remaining = stream_state.bytes.len() - stream_state.offset;
    let read_len = remaining.min(STREAM_CHUNK_BYTES);
    let start = stream_state.offset;
    let end = start + read_len;
    let chunk = stream_state
      .bytes
      .get(start..end)
      .unwrap_or(&[])
      .to_vec();
    stream_state.offset = end;
    if stream_state.offset >= stream_state.bytes.len() {
      stream_state.state = StreamLifecycleState::Closed;
    }

    Ok(ReadOutcome::Chunk(chunk))
  })?;

  match outcome {
    ReadOutcome::Chunk(chunk) => {
      // Create `Uint8Array` value.
      let byte_len = chunk.len();
      let ab = scope.alloc_array_buffer_from_u8_vec(chunk)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

      let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
      scope.push_root(Value::Object(view))?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;

      // Resolve to `{ value, done }`.
      let result = scope.alloc_object()?;
      scope.push_root(Value::Object(result))?;

      let value_key = alloc_key(scope, "value")?;
      let done_key = alloc_key(scope, "done")?;

      scope.define_property(result, value_key, result_data_desc(Value::Object(view)))?;
      scope.define_property(result, done_key, result_data_desc(Value::Bool(false)))?;

      vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        resolve,
        Value::Undefined,
        &[Value::Object(result)],
      )?;
    }
    ReadOutcome::Done => {
      let result = scope.alloc_object()?;
      scope.push_root(Value::Object(result))?;

      let value_key = alloc_key(scope, "value")?;
      let done_key = alloc_key(scope, "done")?;

      scope.define_property(result, value_key, result_data_desc(Value::Undefined))?;
      scope.define_property(result, done_key, result_data_desc(Value::Bool(true)))?;

      vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        resolve,
        Value::Undefined,
        &[Value::Object(result)],
      )?;
    }
    ReadOutcome::Error(msg) => {
      let err = new_type_error_object(scope, &intr, &msg)?;
      scope.push_root(err)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[err])?;
    }
  }

  Ok(promise)
}

fn reader_release_lock_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(reader_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.releaseLock: illegal invocation",
    ));
  };

  with_realm_state_mut(vm, scope, callee, |state| {
    let reader_state = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.releaseLock: illegal invocation",
      ))?;

    let Some(stream_weak) = reader_state.stream.take() else {
      return Ok(Value::Undefined);
    };

    if let Some(stream_state) = state.streams.get_mut(&stream_weak) {
      stream_state.locked = false;
    }

    Ok(Value::Undefined)
  })
}

fn reader_cancel_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(reader_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.cancel: illegal invocation",
    ));
  };

  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("ReadableStream requires intrinsics"))?;

  let outcome = with_realm_state_mut(vm, scope, callee, |state| {
    let reader_state = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.cancel: illegal invocation",
      ))?;
    let Some(stream_weak) = reader_state.stream else {
      return Ok(ReadOutcome::Done);
    };
    let Some(stream_state) = state.streams.get_mut(&stream_weak) else {
      return Ok(ReadOutcome::Done);
    };

    stream_state.state = StreamLifecycleState::Closed;
    stream_state.error_message = None;
    stream_state.bytes.clear();
    stream_state.init = None;
    stream_state.offset = 0;

    Ok(ReadOutcome::Done)
  })?;

  match outcome {
    ReadOutcome::Done | ReadOutcome::Chunk(_) => {
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
    }
    ReadOutcome::Error(msg) => {
      let err = new_type_error_object(scope, &intr, &msg)?;
      scope.push_root(err)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[err])?;
    }
  }

  Ok(promise)
}

pub(crate) fn create_readable_byte_stream_from_bytes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  bytes: Vec<u8>,
) -> Result<GcObject, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.readable_stream_proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state
      .streams
      .insert(WeakGcObject::from(obj), StreamState::new_from_bytes(bytes));
    Ok(())
  })?;

  Ok(obj)
}

pub(crate) fn create_readable_byte_stream_lazy(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  init: impl FnOnce() -> Result<Vec<u8>, VmError> + Send + 'static,
) -> Result<GcObject, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state| Ok(state.readable_stream_proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  with_realm_state_mut(vm, scope, callee, |state| {
    state.streams.insert(
      WeakGcObject::from(obj),
      StreamState::new_lazy(Box::new(init)),
    );
    Ok(())
  })?;

  Ok(obj)
}

/// Returns `true` if `obj` is a `ReadableStream` created by this module's bindings.
///
/// This is used by Fetch BodyInit parsing to avoid treating stream bodies as strings.
pub(crate) fn is_readable_stream_object(vm: &Vm, heap: &Heap, obj: GcObject) -> bool {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());

  let key = WeakGcObject::from(obj);
  let gc_runs = heap.gc_runs();

  if let Some(realm_id) = vm.current_realm() {
    let Some(state) = registry.realms.get_mut(&realm_id) else {
      return false;
    };
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      state.streams.retain(|k, _| k.upgrade(heap).is_some());
      state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }
    return state.streams.contains_key(&key);
  }

  // If we don't have a current realm (e.g. tests calling native handlers directly), fall back to
  // scanning all installed realm states. The number of realms is expected to be small.
  for state in registry.realms.values_mut() {
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      state.streams.retain(|k, _| k.upgrade(heap).is_some());
      state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }
    if state.streams.contains_key(&key) {
      return true;
    }
  }
  false
}

pub fn install_window_streams_bindings(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- ReadableStream ----------------------------------------------------------
  let stream_call_id: NativeFunctionId = vm.register_native_call(readable_stream_ctor_call)?;
  let stream_construct_id: NativeConstructId = vm.register_native_construct(readable_stream_ctor_construct)?;

  let stream_name = scope.alloc_string("ReadableStream")?;
  scope.push_root(Value::String(stream_name))?;
  let stream_ctor = scope.alloc_native_function_with_slots(
    stream_call_id,
    Some(stream_construct_id),
    stream_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(stream_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(stream_ctor, Some(intr.function_prototype()))?;

  let stream_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(stream_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "ReadableStream constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(stream_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(stream_proto, Some(intr.object_prototype()))?;

  let get_reader_call_id: NativeFunctionId = vm.register_native_call(readable_stream_get_reader_native)?;
  let get_reader_name = scope.alloc_string("getReader")?;
  scope.push_root(Value::String(get_reader_name))?;
  let get_reader_fn = scope.alloc_native_function_with_slots(
    get_reader_call_id,
    None,
    get_reader_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(get_reader_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(get_reader_fn, Some(intr.function_prototype()))?;
  let get_reader_key = alloc_key(&mut scope, "getReader")?;
  scope.define_property(stream_proto, get_reader_key, data_desc(Value::Object(get_reader_fn), true))?;

  let cancel_call_id: NativeFunctionId = vm.register_native_call(readable_stream_cancel_native)?;
  let cancel_name = scope.alloc_string("cancel")?;
  scope.push_root(Value::String(cancel_name))?;
  let cancel_fn = scope.alloc_native_function_with_slots(
    cancel_call_id,
    None,
    cancel_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(cancel_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(cancel_fn, Some(intr.function_prototype()))?;
  let cancel_key = alloc_key(&mut scope, "cancel")?;
  scope.define_property(stream_proto, cancel_key, data_desc(Value::Object(cancel_fn), true))?;

  let locked_get_call_id: NativeFunctionId = vm.register_native_call(readable_stream_locked_get_native)?;
  let locked_get_name = scope.alloc_string("get locked")?;
  scope.push_root(Value::String(locked_get_name))?;
  let locked_get_fn = scope.alloc_native_function_with_slots(
    locked_get_call_id,
    None,
    locked_get_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(locked_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(locked_get_fn, Some(intr.function_prototype()))?;
  let locked_key = alloc_key(&mut scope, "locked")?;
  scope.define_property(
    stream_proto,
    locked_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(locked_get_fn),
        set: Value::Undefined,
      },
    },
  )?;

  let to_string_tag = intr.well_known_symbols().to_string_tag;
  let stream_tag_key = PropertyKey::from_symbol(to_string_tag);
  let stream_tag_val = scope.alloc_string("ReadableStream")?;
  scope.push_root(Value::String(stream_tag_val))?;
  scope.define_property(stream_proto, stream_tag_key, data_desc(Value::String(stream_tag_val), false))?;

  let stream_ctor_key = alloc_key(&mut scope, "ReadableStream")?;
  scope.define_property(global, stream_ctor_key, data_desc(Value::Object(stream_ctor), true))?;

  // --- ReadableStreamDefaultReader --------------------------------------------
  let reader_call_id: NativeFunctionId = vm.register_native_call(reader_ctor_call)?;
  let reader_construct_id: NativeConstructId = vm.register_native_construct(reader_ctor_construct)?;

  let reader_name = scope.alloc_string("ReadableStreamDefaultReader")?;
  scope.push_root(Value::String(reader_name))?;
  let reader_ctor = scope.alloc_native_function_with_slots(
    reader_call_id,
    Some(reader_construct_id),
    reader_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(reader_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(reader_ctor, Some(intr.function_prototype()))?;

  let reader_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(reader_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "ReadableStreamDefaultReader constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(reader_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(reader_proto, Some(intr.object_prototype()))?;

  let read_call_id: NativeFunctionId = vm.register_native_call(reader_read_native)?;
  let read_name = scope.alloc_string("read")?;
  scope.push_root(Value::String(read_name))?;
  let read_fn = scope.alloc_native_function_with_slots(
    read_call_id,
    None,
    read_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(read_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(read_fn, Some(intr.function_prototype()))?;
  let read_key = alloc_key(&mut scope, "read")?;
  scope.define_property(reader_proto, read_key, data_desc(Value::Object(read_fn), true))?;

  let release_call_id: NativeFunctionId = vm.register_native_call(reader_release_lock_native)?;
  let release_name = scope.alloc_string("releaseLock")?;
  scope.push_root(Value::String(release_name))?;
  let release_fn = scope.alloc_native_function_with_slots(
    release_call_id,
    None,
    release_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(release_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(release_fn, Some(intr.function_prototype()))?;
  let release_key = alloc_key(&mut scope, "releaseLock")?;
  scope.define_property(reader_proto, release_key, data_desc(Value::Object(release_fn), true))?;

  let reader_cancel_call_id: NativeFunctionId = vm.register_native_call(reader_cancel_native)?;
  let reader_cancel_name = scope.alloc_string("cancel")?;
  scope.push_root(Value::String(reader_cancel_name))?;
  let reader_cancel_fn = scope.alloc_native_function_with_slots(
    reader_cancel_call_id,
    None,
    reader_cancel_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(reader_cancel_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(reader_cancel_fn, Some(intr.function_prototype()))?;
  let reader_cancel_key = alloc_key(&mut scope, "cancel")?;
  scope.define_property(
    reader_proto,
    reader_cancel_key,
    data_desc(Value::Object(reader_cancel_fn), true),
  )?;

  let reader_tag_key = PropertyKey::from_symbol(to_string_tag);
  let reader_tag_val = scope.alloc_string("ReadableStreamDefaultReader")?;
  scope.push_root(Value::String(reader_tag_val))?;
  scope.define_property(reader_proto, reader_tag_key, data_desc(Value::String(reader_tag_val), false))?;

  let reader_ctor_key = alloc_key(&mut scope, "ReadableStreamDefaultReader")?;
  scope.define_property(global, reader_ctor_key, data_desc(Value::Object(reader_ctor), true))?;

  // Register per-realm state.
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    StreamRealmState {
      readable_stream_proto: stream_proto,
      reader_proto,
      streams: HashMap::new(),
      readers: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_streams_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry()
    .lock()
    .unwrap_or_else(|err| err.into_inner());
  registry.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use vm_js::PromiseState;

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn read_result_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
    let key = alloc_key(scope, name)?;
    Ok(
      scope
        .heap()
        .object_get_own_data_property_value(obj, &key)?
        .unwrap_or(Value::Undefined),
    )
  }

  #[test]
  fn readable_stream_is_installed_and_constructible() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ty = realm.exec_script("typeof ReadableStream")?;
    assert_eq!(get_string(realm.heap(), ty), "function");

    let ok = realm.exec_script("new ReadableStream() instanceof ReadableStream")?;
    assert_eq!(ok, Value::Bool(true));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn host_created_stream_from_bytes_can_be_read_via_js() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Grab the ctor so we can pass a callee that carries the realm ID slot if no realm is active.
    let ctor_val = realm.exec_script("ReadableStream")?;
    let Value::Object(ctor_obj) = ctor_val else {
      return Err(VmError::InvariantViolation(
        "ReadableStream must be a function object",
      ));
    };

    // Create the stream in Rust and expose it on the global object as `stream`.
    {
      let (vm, realm_obj, heap) = realm.vm_realm_and_heap_mut();
      let global = realm_obj.global_object();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(global))?;

      let stream = create_readable_byte_stream_from_bytes(vm, &mut scope, ctor_obj, b"hi".to_vec())?;
      scope.push_root(Value::Object(stream))?;

      let stream_key = alloc_key(&mut scope, "stream")?;
      scope.define_property(global, stream_key, data_desc(Value::Object(stream), true))?;
    }

    // Keep a reader in global state across multiple exec_script calls.
    let _ = realm.exec_script("globalThis.reader = stream.getReader();")?;

    let p1 = realm.exec_script("reader.read()")?;
    let Value::Object(p1_obj) = p1 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p1_obj)?, PromiseState::Fulfilled);
    let Some(result1_val) = realm.heap().promise_result(p1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result1_obj) = result1_val else {
      return Err(VmError::InvariantViolation("read() must resolve to an object"));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done1 = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done1, Value::Bool(false));
      let value1 = read_result_prop(&mut scope, result1_obj, "value")?;
      let Value::Object(value1_obj) = value1 else {
        return Err(VmError::InvariantViolation("read() result.value must be an object"));
      };
      assert!(scope.heap().is_uint8_array_object(value1_obj));
      assert_eq!(scope.heap().uint8_array_data(value1_obj)?, b"hi");
    }

    let p2 = realm.exec_script("reader.read()")?;
    let Value::Object(p2_obj) = p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p2_obj)?, PromiseState::Fulfilled);
    let Some(result2_val) = realm.heap().promise_result(p2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result2_obj) = result2_val else {
      return Err(VmError::InvariantViolation("read() must resolve to an object"));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done2 = read_result_prop(&mut scope, result2_obj, "done")?;
      assert_eq!(done2, Value::Bool(true));
      let value2 = read_result_prop(&mut scope, result2_obj, "value")?;
      assert!(matches!(value2, Value::Undefined));
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn lazy_stream_does_not_execute_init_until_first_read() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ctor_val = realm.exec_script("ReadableStream")?;
    let Value::Object(ctor_obj) = ctor_val else {
      return Err(VmError::InvariantViolation(
        "ReadableStream must be a function object",
      ));
    };

    let init_calls = Arc::new(AtomicUsize::new(0));
    let init_calls_for_stream = Arc::clone(&init_calls);

    {
      let (vm, realm_obj, heap) = realm.vm_realm_and_heap_mut();
      let global = realm_obj.global_object();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(global))?;

      let stream = create_readable_byte_stream_lazy(vm, &mut scope, ctor_obj, move || {
        init_calls_for_stream.fetch_add(1, Ordering::SeqCst);
        Ok(b"ok".to_vec())
      })?;
      scope.push_root(Value::Object(stream))?;

      let stream_key = alloc_key(&mut scope, "lazyStream")?;
      scope.define_property(global, stream_key, data_desc(Value::Object(stream), true))?;
    }

    assert_eq!(init_calls.load(Ordering::SeqCst), 0);

    let _ = realm.exec_script("globalThis.lazyReader = lazyStream.getReader();")?;
    let p = realm.exec_script("lazyReader.read()")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Fulfilled);
    assert_eq!(init_calls.load(Ordering::SeqCst), 1);

    // Second read should not re-run init.
    let _ = realm.exec_script("lazyReader.read()")?;
    assert_eq!(init_calls.load(Ordering::SeqCst), 1);

    realm.teardown();
    Ok(())
  }
}
