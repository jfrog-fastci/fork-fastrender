//! Minimal WHATWG Streams implementation (`ReadableStream`, `WritableStream`, `TransformStream`)
//! for `vm-js` Window realms.
//!
//! This is a deliberately small subset used by streaming consumers in Fetch/Blob-like APIs and by
//! encoding helpers like `TextEncoderStream`.
//!
//! The `ReadableStream` implementation is primarily byte-oriented: host-created streams and
//! `TransformStream.readable` enqueue `Uint8Array` chunks.
//!
//! A minimal underlying source/controller API is also provided for real-world scripts that create
//! streams via `new ReadableStream({ start(controller) { ... } })` and later call
//! `controller.enqueue(string)` / `controller.close()`.
//!
//! Notably absent:
//! - `ReadableStream` underlying source/controller APIs beyond `start` + `enqueue/close/error`
//!   (no `pull`, backpressure, etc.)
//! - BYOB readers
//! - teeing/backpressure
//!
//! The goal is to provide just enough surface area for real-world code that expects streams
//! constructors to exist and for host-owned byte sources to be consumed via
//! `readable.getReader().read()`.

use std::char;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use vm_js::{
  new_promise_capability_with_host_and_hooks, new_type_error_object,
  perform_promise_then_with_host_and_hooks, promise_resolve_with_host_and_hooks, GcObject, Heap,
  HostSlots, Intrinsics, NativeConstructId, NativeFunctionId, PromiseCapability,
  PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, RootId, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks, WeakGcObject,
};

const STREAM_REALM_ID_SLOT: usize = 0;
const READER_STREAM_REF_KEY: &str = "__fastrender_readable_stream_reader_stream_ref";

// Brand stream wrappers as platform objects via HostSlots so structuredClone rejects them with
// DataCloneError (streams are not structured-cloneable without special transfer support).
const READABLE_STREAM_HOST_TAG: u64 = 0x5245_4144_5354_524D; // "READSTRM"
const READABLE_STREAM_DEFAULT_READER_HOST_TAG: u64 = 0x5253_5245_4144_4552; // "RSREADER"
const READABLE_STREAM_DEFAULT_CONTROLLER_HOST_TAG: u64 = 0x5253_434E_5452_4C52; // "RSCNTRLR"
const WRITABLE_STREAM_HOST_TAG: u64 = 0x5752_4954_5354_524D; // "WRITSTRM"
const WRITABLE_STREAM_DEFAULT_WRITER_HOST_TAG: u64 = 0x5753_5752_4954_4552; // "WSWRITER"
const TRANSFORM_STREAM_HOST_TAG: u64 = 0x5452_4E53_5354_524D; // "TRNSSTRM"
const TRANSFORM_STREAM_DEFAULT_CONTROLLER_HOST_TAG: u64 = 0x5453_434E_5452_4C52; // "TSCNTRLR"
const TRANSFORM_STREAM_SINK_HOST_TAG: u64 = 0x5453_5349_4E4B_5F5F; // "TSSINK__"

/// Maximum bytes returned by a single `reader.read()` call.
///
/// This is an internal chunking detail; it bounds per-read allocation while still being reasonably
/// large for network payloads.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Upper bound for a single string chunk enqueued via a `ReadableStream` controller.
///
/// This mirrors the 32MiB cap used by `TextEncoder.encode` / `TextEncoderStream` to avoid
/// unbounded host allocations when scripts enqueue attacker-controlled strings.
const MAX_READABLE_STREAM_STRING_CHUNK_BYTES: usize = 32 * 1024 * 1024;

// --- Hidden, internal property keys -----------------------------------------------------------

const READABLE_STREAM_READER_PENDING_RESOLVE_KEY: &str =
  "__fastrender_readable_stream_pending_read_resolve";
const READABLE_STREAM_READER_PENDING_REJECT_KEY: &str =
  "__fastrender_readable_stream_pending_read_reject";

const READABLE_STREAM_CONTROLLER_BRAND_KEY: &str =
  "__fastrender_readable_stream_default_controller";
const READABLE_STREAM_CONTROLLER_STREAM_KEY: &str =
  "__fastrender_readable_stream_default_controller_stream";

const WRITABLE_STREAM_BRAND_KEY: &str = "__fastrender_writable_stream";
const WRITABLE_STREAM_SINK_KEY: &str = "__fastrender_writable_stream_sink";
const WRITABLE_STREAM_SINK_WRITE_KEY: &str = "__fastrender_writable_stream_sink_write";
const WRITABLE_STREAM_SINK_CLOSE_KEY: &str = "__fastrender_writable_stream_sink_close";
const WRITABLE_STREAM_SINK_ABORT_KEY: &str = "__fastrender_writable_stream_sink_abort";

const WRITABLE_STREAM_WRITER_BRAND_KEY: &str = "__fastrender_writable_stream_default_writer";
const WRITABLE_STREAM_WRITER_STREAM_KEY: &str =
  "__fastrender_writable_stream_default_writer_stream";

const TRANSFORM_CONTROLLER_BRAND_KEY: &str = "__fastrender_transform_stream_default_controller";
const TRANSFORM_CONTROLLER_READABLE_STREAM_KEY: &str =
  "__fastrender_transform_stream_default_controller_readable_stream";

const TRANSFORM_SINK_BRAND_KEY: &str = "__fastrender_transform_stream_sink";
const TRANSFORM_SINK_TRANSFORMER_KEY: &str = "__fastrender_transform_stream_sink_transformer";
const TRANSFORM_SINK_TRANSFORM_KEY: &str = "__fastrender_transform_stream_sink_transform";
const TRANSFORM_SINK_FLUSH_KEY: &str = "__fastrender_transform_stream_sink_flush";
const TRANSFORM_SINK_CONTROLLER_KEY: &str = "__fastrender_transform_stream_sink_controller";

const WRITABLE_STREAM_GET_WRITER_SLOT_WRITER_PROTO: usize = 1;

const TRANSFORM_STREAM_CTOR_SLOT_SINK_WRITE_FN: usize = 1;
const TRANSFORM_STREAM_CTOR_SLOT_SINK_CLOSE_FN: usize = 2;
const TRANSFORM_STREAM_CTOR_SLOT_SINK_ABORT_FN: usize = 3;
const TRANSFORM_STREAM_CTOR_SLOT_CONTROLLER_PROTO: usize = 4;

const READABLE_STREAM_PIPE_THROUGH_SLOT_READER: usize = 1;
const READABLE_STREAM_PIPE_THROUGH_SLOT_WRITER: usize = 2;

const READABLE_STREAM_PIPE_TO_SLOT_READER: usize = 1;
const READABLE_STREAM_PIPE_TO_SLOT_WRITER: usize = 2;
const READABLE_STREAM_PIPE_TO_SLOT_RESOLVE: usize = 3;
const READABLE_STREAM_PIPE_TO_SLOT_REJECT: usize = 4;
const READABLE_STREAM_PIPE_TO_SLOT_PREVENT_CLOSE: usize = 5;

fn chunk_sizes_for_len(mut len: usize) -> VecDeque<usize> {
  let mut queue = VecDeque::new();
  while len > 0 {
    let take = len.min(STREAM_CHUNK_BYTES);
    queue.push_back(take);
    len -= take;
  }
  queue
}

fn push_chunk_sizes(queue: &mut VecDeque<usize>, len: usize) {
  if len == 0 {
    queue.push_back(0);
    return;
  }

  let mut remaining = len;
  while remaining > 0 {
    let take = remaining.min(STREAM_CHUNK_BYTES);
    queue.push_back(take);
    remaining -= take;
  }
}

fn utf8_len_from_utf16_units(units: &[u16]) -> Result<usize, VmError> {
  let mut len: usize = 0;
  for unit in char::decode_utf16(units.iter().copied()) {
    let ch = unit.unwrap_or('\u{FFFD}');
    len = len.checked_add(ch.len_utf8()).ok_or(VmError::OutOfMemory)?;
  }
  Ok(len)
}

fn utf16_units_to_utf8_string_lossy(units: &[u16], byte_len: usize) -> Result<String, VmError> {
  let mut out = String::new();
  out
    .try_reserve_exact(byte_len)
    .map_err(|_| VmError::OutOfMemory)?;
  for unit in char::decode_utf16(units.iter().copied()) {
    let ch = unit.unwrap_or('\u{FFFD}');
    out.push(ch);
  }
  debug_assert_eq!(out.len(), byte_len);
  Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamLifecycleState {
  Readable,
  Closed,
  Errored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
  Bytes,
  Strings,
}

type LazyInit = Box<dyn FnOnce() -> Result<Vec<u8>, VmError> + Send + 'static>;

#[derive(Debug, Clone, Copy)]
struct PendingReadRoots {
  reader: RootId,
  stream: RootId,
}

struct StreamState {
  locked: bool,
  state: StreamLifecycleState,
  /// `true` if no more bytes will be enqueued into the stream.
  ///
  /// For fixed-byte streams (e.g. `create_readable_byte_stream_from_bytes`), this is `true` from
  /// creation.
  ///
  /// For dynamic streams (e.g. `new ReadableStream()` or `TransformStream.readable`), this is set
  /// by internal close/terminate paths.
  close_requested: bool,
  error_message: Option<String>,
  bytes: Vec<u8>,
  /// Queue of chunk sizes (in bytes) remaining in the stream.
  ///
  /// For dynamic streams (`TransformStream.readable`), this preserves enqueue boundaries and
  /// ensures empty chunks (`Uint8Array(0)`) still resolve pending reads.
  queue: VecDeque<usize>,
  /// Queue of string chunks (only used when `kind == StreamKind::Strings`).
  strings: VecDeque<String>,
  init: Option<LazyInit>,
  offset: usize,
  /// A pending `reader.read()` call waiting for bytes (only used for dynamic streams).
  pending_reader: Option<WeakGcObject>,
  pending_read_roots: Option<PendingReadRoots>,
  kind: StreamKind,
}

impl StreamState {
  fn new_empty() -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      close_requested: false,
      error_message: None,
      bytes: Vec::new(),
      queue: VecDeque::new(),
      strings: VecDeque::new(),
      init: None,
      offset: 0,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Bytes,
    }
  }

  fn new_empty_strings() -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      close_requested: false,
      error_message: None,
      bytes: Vec::new(),
      queue: VecDeque::new(),
      strings: VecDeque::new(),
      init: None,
      offset: 0,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Strings,
    }
  }

  fn new_from_bytes(bytes: Vec<u8>) -> Self {
    let queue = chunk_sizes_for_len(bytes.len());
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      close_requested: true,
      error_message: None,
      bytes,
      queue,
      strings: VecDeque::new(),
      init: None,
      offset: 0,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Bytes,
    }
  }

  fn new_lazy(init: LazyInit) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      close_requested: true,
      error_message: None,
      bytes: Vec::new(),
      queue: VecDeque::new(),
      strings: VecDeque::new(),
      init: Some(init),
      offset: 0,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Bytes,
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
  readable_stream_controller_proto: GcObject,
  reader_proto: GcObject,
  writable_stream_proto: GcObject,
  writer_proto: GcObject,
  transform_stream_proto: GcObject,
  transform_controller_proto: GcObject,
  transform_close_after_flush_fulfilled_call_id: NativeFunctionId,
  transform_close_after_flush_rejected_call_id: NativeFunctionId,
  readable_stream_pipe_through_read_fulfilled_call_id: NativeFunctionId,
  readable_stream_pipe_through_read_rejected_call_id: NativeFunctionId,
  readable_stream_pipe_through_write_fulfilled_call_id: NativeFunctionId,
  readable_stream_pipe_through_write_rejected_call_id: NativeFunctionId,
  readable_stream_pipe_to_read_fulfilled_call_id: NativeFunctionId,
  readable_stream_pipe_to_read_rejected_call_id: NativeFunctionId,
  readable_stream_pipe_to_write_fulfilled_call_id: NativeFunctionId,
  readable_stream_pipe_to_write_rejected_call_id: NativeFunctionId,
  readable_stream_pipe_to_close_fulfilled_call_id: NativeFunctionId,
  readable_stream_pipe_to_close_rejected_call_id: NativeFunctionId,
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

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  data_desc(value, false)
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

fn get_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root inputs while allocating the key (`alloc_key` can trigger GC).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn require_intrinsics(vm: &Vm, err: &'static str) -> Result<Intrinsics, VmError> {
  vm.intrinsics().ok_or(VmError::Unimplemented(err))
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

fn realm_id_for_binding_call(vm: &Vm, heap: &Heap, callee: GcObject) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }

  let slots = heap.get_function_native_slots(callee)?;
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
  scope: &Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut StreamRealmState, &Heap) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let heap = scope.heap();
  let realm_id = realm_id_for_binding_call(vm, heap, callee)?;

  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  let state = registry
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "ReadableStream bindings used before install_window_streams_bindings",
    ))?;

  // Opportunistically sweep dead objects when GC has run.
  let gc_runs = heap.gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    state.streams.retain(|k, _| k.upgrade(heap).is_some());
    state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state, heap)
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
  Err(VmError::TypeError(
    "ReadableStream constructor requires 'new'",
  ))
}

fn readable_stream_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.readable_stream_proto)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: READABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  let underlying_source = args.get(0).copied().unwrap_or(Value::Undefined);

  let mut start_fn: Value = Value::Undefined;
  let mut underlying_source_obj: Option<GcObject> = None;

  if let Value::Object(source_obj) = underlying_source {
    underlying_source_obj = Some(source_obj);

    // Root the source across `alloc_key`.
    scope.push_root(underlying_source)?;
    let start_key = alloc_key(scope, "start")?;
    let start_val = vm.get_with_host_and_hooks(host, scope, hooks, source_obj, start_key)?;
    if scope.heap().is_callable(start_val)? {
      start_fn = start_val;
    }
  }

  let has_start = !matches!(start_fn, Value::Undefined);
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    state.streams.insert(
      WeakGcObject::from(obj),
      if has_start {
        StreamState::new_empty_strings()
      } else {
        StreamState::new_empty()
      },
    );
    Ok(())
  })?;

  if has_start {
    // Root the start function and underlying source across controller allocation. In the common case
    // `start` is a data property on `underlyingSource` and remains reachable, but it could also be a
    // getter that returns an otherwise-unreachable callable.
    scope.push_root(start_fn)?;
    if let Some(source_obj) = underlying_source_obj {
      scope.push_root(Value::Object(source_obj))?;
    }

    let controller_proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok(state.readable_stream_controller_proto)
    })?;

    let controller_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(controller_obj))?;
    scope
      .heap_mut()
      .object_set_prototype(controller_obj, Some(controller_proto))?;
    scope.heap_mut().object_set_host_slots(
      controller_obj,
      HostSlots {
        a: READABLE_STREAM_DEFAULT_CONTROLLER_HOST_TAG,
        b: 0,
      },
    )?;

    set_data_prop(
      scope,
      controller_obj,
      READABLE_STREAM_CONTROLLER_BRAND_KEY,
      Value::Bool(true),
      false,
    )?;
    set_data_prop(
      scope,
      controller_obj,
      READABLE_STREAM_CONTROLLER_STREAM_KEY,
      Value::Object(obj),
      false,
    )?;

    let receiver = match underlying_source_obj {
      Some(source_obj) => Value::Object(source_obj),
      None => Value::Undefined,
    };

    vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      start_fn,
      receiver,
      &[Value::Object(controller_obj)],
    )?;
  }

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
  let stream_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_HOST_TAG,
    "ReadableStream.getReader: illegal invocation",
  )?;

  let reader_proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError(
        "ReadableStream.getReader: illegal invocation",
      ))?;
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
  scope.heap_mut().object_set_host_slots(
    reader_obj,
    HostSlots {
      a: READABLE_STREAM_DEFAULT_READER_HOST_TAG,
      b: 0,
    },
  )?;

  // Keep the stream alive as long as the reader is alive (the spec stores this as an internal
  // slot, which should be a strong reference).
  scope.push_root(Value::Object(stream_obj))?;
  let stream_ref_key = alloc_key(scope, READER_STREAM_REF_KEY)?;
  scope.define_property(
    reader_obj,
    stream_ref_key,
    data_desc(Value::Object(stream_obj), false),
  )?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
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
  let stream_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_HOST_TAG,
    "ReadableStream.cancel: illegal invocation",
  )?;

  // Always return a Promise (spec shape), even though we resolve synchronously.
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  scope.push_root(cap.reject)?;

  // Perform the cancel synchronously, but reject if the stream is locked.
  let cancel_result = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError(
        "ReadableStream.cancel: illegal invocation",
      ))?;
    if stream_state.locked {
      return Err(VmError::TypeError("ReadableStream is locked"));
    }

    // Lazy streams created by host APIs (e.g. Fetch `Response.body`) may need to run their init
    // closure on cancel so host-side resources are released and the stream is considered
    // "disturbed" even if no reader ever consumes it.
    if let Some(init) = stream_state.init.take() {
      let _ = init();
    }

    stream_state.state = StreamLifecycleState::Closed;
    stream_state.close_requested = true;
    stream_state.error_message = None;
    stream_state.bytes.clear();
    stream_state.queue.clear();
    stream_state.strings.clear();
    stream_state.init = None;
    stream_state.offset = 0;
    stream_state.pending_reader = None;
    stream_state.pending_read_roots = None;
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
  let stream_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_HOST_TAG,
    "ReadableStream.locked: illegal invocation",
  )?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state =
      state
        .streams
        .get(&WeakGcObject::from(stream_obj))
        .ok_or(VmError::TypeError(
          "ReadableStream.locked: illegal invocation",
        ))?;
    Ok(Value::Bool(stream_state.locked))
  })
}

fn readable_stream_pipe_through_read_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough read callback missing reader slot",
      ))
    }
  };
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough read callback missing writer slot",
      ))
    }
  };
  let realm_slot = slots
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);

  let result = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(result_obj) = result else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: read() did not return an object",
    ));
  };

  // Root result object while allocating keys / accessing properties.
  scope.push_root(Value::Object(result_obj))?;
  let done_key = alloc_key(scope, "done")?;
  let done_val = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, done_key)?;
  let done = scope.heap().to_boolean(done_val)?;

  if done {
    // writer.close()
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(writer_obj))?;
    let close_key = alloc_key(&mut scope, "close")?;
    let close_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, close_key)?;
    let _ = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      close_fn,
      Value::Object(writer_obj),
      &[],
    )?;
    return Ok(Value::Undefined);
  }

  let value_key = alloc_key(scope, "value")?;
  let chunk = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, value_key)?;

  // writer.write(chunk)
  let write_promise = {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(writer_obj))?;
    scope.push_root(chunk)?;
    let write_key = alloc_key(&mut scope, "write")?;
    let write_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, write_key)?;
    vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      write_fn,
      Value::Object(writer_obj),
      &[chunk],
    )?
  };

  let Value::Object(write_promise_obj) = write_promise else {
    return Err(VmError::InvariantViolation(
      "WritableStreamDefaultWriter.write must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_through_write_fulfilled_call_id,
        state.readable_stream_pipe_through_write_rejected_call_id,
      ))
    })?;

  // Root reader, writer, and promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(write_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeThrough write fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeThrough write rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(write_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(Value::Undefined)
}

fn readable_stream_pipe_through_read_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough read rejection callback missing writer slot",
      ))
    }
  };

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);

  // Best-effort `writer.abort(reason)`; ignore rejections (we're already in an error path).
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(reason)?;
  let abort_key = alloc_key(scope, "abort")?;
  let abort_fn = vm.get_with_host_and_hooks(host, scope, hooks, writer_obj, abort_key)?;
  if scope.heap().is_callable(abort_fn)? {
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      abort_fn,
      Value::Object(writer_obj),
      &[reason],
    );
  }

  Ok(Value::Undefined)
}

fn readable_stream_pipe_through_write_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough write callback missing reader slot",
      ))
    }
  };
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough write callback missing writer slot",
      ))
    }
  };
  let realm_slot = slots
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);

  // Start the next `reader.read()`.
  let read_promise = reader_read_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  )?;
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_through_read_fulfilled_call_id,
        state.readable_stream_pipe_through_read_rejected_call_id,
      ))
    })?;

  // Root reader/writer/promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeThrough read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeThrough read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(read_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(Value::Undefined)
}

fn readable_stream_pipe_through_write_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_THROUGH_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeThrough write rejection callback missing reader slot",
      ))
    }
  };

  // Best-effort cancellation of the source stream.
  let _ = reader_cancel_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  )?;

  // Nothing else to do; the destination write failed.
  let _reason = args.get(0).copied().unwrap_or(Value::Undefined);
  Ok(Value::Undefined)
}

fn readable_stream_pipe_through_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: illegal invocation",
    ));
  };
  // Root the source stream across allocations in this helper. The VM call frame roots `this`, but we
  // also invoke other native helpers directly from Rust, which can allocate/GC before they root
  // their receivers.
  scope.push_root(Value::Object(stream_obj))?;

  // Ensure this is one of our streams (and not e.g. an arbitrary object inheriting the prototype).
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    state
      .streams
      .contains_key(&WeakGcObject::from(stream_obj))
      .then_some(())
      .ok_or(VmError::TypeError(
        "ReadableStream.pipeThrough: illegal invocation",
      ))
  })?;

  let transform = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(transform_obj) = transform else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough expects a transform object",
    ));
  };

  // Extract `{ readable, writable }` from the transform.
  scope.push_root(Value::Object(transform_obj))?;
  let readable_key = alloc_key(scope, "readable")?;
  let writable_key = alloc_key(scope, "writable")?;
  let readable_val = vm.get_with_host_and_hooks(host, scope, hooks, transform_obj, readable_key)?;
  let writable_val = vm.get_with_host_and_hooks(host, scope, hooks, transform_obj, writable_key)?;

  let Value::Object(readable_obj) = readable_val else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: transform.readable is not an object",
    ));
  };
  let Value::Object(writable_obj) = writable_val else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: transform.writable is not an object",
    ));
  };

  // Acquire a writer from `transform.writable`.
  scope.push_root(Value::Object(writable_obj))?;
  let get_writer_key = alloc_key(scope, "getWriter")?;
  let get_writer_fn =
    vm.get_with_host_and_hooks(host, scope, hooks, writable_obj, get_writer_key)?;
  if !scope.heap().is_callable(get_writer_fn)? {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: transform.writable.getWriter is not callable",
    ));
  }
  let writer_val = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    get_writer_fn,
    Value::Object(writable_obj),
    &[],
  )?;
  let Value::Object(writer_obj) = writer_val else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: getWriter did not return an object",
    ));
  };
  // Root writer across subsequent allocations (not otherwise reachable until we capture it in the
  // Promise reaction callbacks).
  scope.push_root(Value::Object(writer_obj))?;

  // Lock the source stream and start an asynchronous pump:
  //
  // - `source.getReader().read().then(write-to-writable, abort-on-error)`
  // - Each successful `write()` schedules the next `read()`.
  let reader_val = readable_stream_get_reader_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(stream_obj),
    &[],
  )?;
  let Value::Object(reader_obj) = reader_val else {
    return Err(VmError::InvariantViolation(
      "ReadableStream.getReader must return an object",
    ));
  };
  // Root reader across subsequent allocations (not otherwise reachable until we capture it in the
  // Promise reaction callbacks).
  scope.push_root(Value::Object(reader_obj))?;

  let read_promise = reader_read_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  )?;
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_through_read_fulfilled_call_id,
        state.readable_stream_pipe_through_read_rejected_call_id,
      ))
    })?;

  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root reader, writer, and promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeThrough read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeThrough read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(read_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(Value::Object(readable_obj))
}

fn readable_stream_pipe_to_close_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let resolve = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_RESOLVE)
    .copied()
    .unwrap_or(Value::Undefined);
  vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_close_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(reason)?;
  vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_read_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo read callback missing reader slot",
      ))
    }
  };
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo read callback missing writer slot",
      ))
    }
  };
  let resolve = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_RESOLVE)
    .copied()
    .unwrap_or(Value::Undefined);
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);
  let prevent_close = matches!(
    slots
      .get(READABLE_STREAM_PIPE_TO_SLOT_PREVENT_CLOSE)
      .copied()
      .unwrap_or(Value::Bool(false)),
    Value::Bool(true)
  );
  let realm_slot = slots
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);

  let result = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(result_obj) = result else {
    let reason = vm_error_to_rejection_value(
      vm,
      scope,
      VmError::TypeError("ReadableStream.pipeTo: read() did not return an object"),
    )?;
    // Best-effort `writer.abort(reason)`.
    let _ = readable_stream_pipe_to_read_rejected_native(
      vm,
      scope,
      host,
      hooks,
      callee,
      Value::Undefined,
      &[reason],
    );
    // `readable_stream_pipe_to_read_rejected_native` already rejected the pipeTo promise.
    return Ok(Value::Undefined);
  };

  // Root result object while allocating keys / accessing properties.
  scope.push_root(Value::Object(result_obj))?;
  let done_key = alloc_key(scope, "done")?;
  let done_val = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, done_key)?;
  let done = scope.heap().to_boolean(done_val)?;

  if done {
    if prevent_close {
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
      return Ok(Value::Undefined);
    }

    // writer.close()
    let close_promise = (|| {
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(writer_obj))?;
      let close_key = alloc_key(&mut scope, "close")?;
      let close_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, close_key)?;
      let close_val = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        close_fn,
        Value::Object(writer_obj),
        &[],
      )?;
      promise_resolve_with_host_and_hooks(vm, &mut scope, host, hooks, close_val)
    })();

    let close_promise = match close_promise {
      Ok(p) => p,
      Err(err) => {
        let reason = vm_error_to_rejection_value(vm, scope, err)?;
        scope.push_root(reason)?;
        vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
        return Ok(Value::Undefined);
      }
    };

    let Value::Object(close_promise_obj) = close_promise else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.pipeTo: close() did not return a Promise",
      ));
    };

    let (fulfilled_call_id, rejected_call_id) =
      with_realm_state_mut(vm, scope, callee, |state, _heap| {
        Ok((
          state.readable_stream_pipe_to_close_fulfilled_call_id,
          state.readable_stream_pipe_to_close_rejected_call_id,
        ))
      })?;

    // Root captured values + promise across callback allocation.
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(reader_obj))?;
    scope.push_root(Value::Object(writer_obj))?;
    scope.push_root(resolve)?;
    scope.push_root(reject)?;
    scope.push_root(Value::Bool(prevent_close))?;
    scope.push_root(close_promise)?;

    let on_fulfilled_name = scope.alloc_string("ReadableStream pipeTo close fulfilled")?;
    scope.push_root(Value::String(on_fulfilled_name))?;
    let on_fulfilled = scope.alloc_native_function_with_slots(
      fulfilled_call_id,
      None,
      on_fulfilled_name,
      1,
      &[
        realm_slot,
        Value::Object(reader_obj),
        Value::Object(writer_obj),
        resolve,
        reject,
        Value::Bool(prevent_close),
      ],
    )?;
    scope.push_root(Value::Object(on_fulfilled))?;

    let on_rejected_name = scope.alloc_string("ReadableStream pipeTo close rejected")?;
    scope.push_root(Value::String(on_rejected_name))?;
    let on_rejected = scope.alloc_native_function_with_slots(
      rejected_call_id,
      None,
      on_rejected_name,
      1,
      &[
        realm_slot,
        Value::Object(reader_obj),
        Value::Object(writer_obj),
        resolve,
        reject,
        Value::Bool(prevent_close),
      ],
    )?;
    scope.push_root(Value::Object(on_rejected))?;

    let derived = perform_promise_then_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      Value::Object(close_promise_obj),
      Some(Value::Object(on_fulfilled)),
      Some(Value::Object(on_rejected)),
    )?;
    mark_promise_handled(&mut scope, derived)?;

    return Ok(Value::Undefined);
  }

  let value_key = alloc_key(scope, "value")?;
  let chunk = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, value_key)?;

  // writer.write(chunk)
  let write_promise = (|| {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(writer_obj))?;
    scope.push_root(chunk)?;
    let write_key = alloc_key(&mut scope, "write")?;
    let write_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, write_key)?;
    let write_val = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      write_fn,
      Value::Object(writer_obj),
      &[chunk],
    )?;
    promise_resolve_with_host_and_hooks(vm, &mut scope, host, hooks, write_val)
  })();

  let write_promise = match write_promise {
    Ok(p) => p,
    Err(err) => {
      let reason = vm_error_to_rejection_value(vm, scope, err)?;
      // Best-effort cancellation of the source stream.
      if let Ok(cancel_promise) = reader_cancel_native(
        vm,
        scope,
        host,
        hooks,
        callee,
        Value::Object(reader_obj),
        &[],
      ) {
        let _ = mark_promise_handled(scope, cancel_promise);
      }
      scope.push_root(reason)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
      return Ok(Value::Undefined);
    }
  };

  let Value::Object(write_promise_obj) = write_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStream.pipeTo: write() did not return a Promise",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_to_write_fulfilled_call_id,
        state.readable_stream_pipe_to_write_rejected_call_id,
      ))
    })?;

  // Root captured values + promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(resolve)?;
  scope.push_root(reject)?;
  scope.push_root(Value::Bool(prevent_close))?;
  scope.push_root(write_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeTo write fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeTo write rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(write_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_read_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo read rejection callback missing writer slot",
      ))
    }
  };
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);

  // Best-effort `writer.abort(reason)`; ignore failures.
  let abort_promise = (|| {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(writer_obj))?;
    scope.push_root(reason)?;
    let abort_key = alloc_key(&mut scope, "abort")?;
    let abort_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, abort_key)?;
    if !scope.heap().is_callable(abort_fn)? {
      return Ok(Value::Undefined);
    }
    vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      abort_fn,
      Value::Object(writer_obj),
      &[reason],
    )
  })();
  if let Ok(abort_val) = abort_promise {
    scope.push_root(abort_val)?;
    if let Ok(p) = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, abort_val) {
      let _ = mark_promise_handled(scope, p);
    }
  }

  scope.push_root(reason)?;
  vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;

  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_write_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo write callback missing reader slot",
      ))
    }
  };
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo write callback missing writer slot",
      ))
    }
  };
  let resolve = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_RESOLVE)
    .copied()
    .unwrap_or(Value::Undefined);
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);
  let prevent_close = matches!(
    slots
      .get(READABLE_STREAM_PIPE_TO_SLOT_PREVENT_CLOSE)
      .copied()
      .unwrap_or(Value::Bool(false)),
    Value::Bool(true)
  );
  let realm_slot = slots
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);

  // Start the next `reader.read()`.
  let read_promise = reader_read_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  );
  let read_promise = match read_promise {
    Ok(p) => p,
    Err(err) => {
      let reason = vm_error_to_rejection_value(vm, scope, err)?;
      // Best-effort `writer.abort(reason)`.
      let _ = readable_stream_pipe_to_read_rejected_native(
        vm,
        scope,
        host,
        hooks,
        callee,
        Value::Undefined,
        &[reason],
      );
      // `readable_stream_pipe_to_read_rejected_native` already rejected the pipeTo promise.
      return Ok(Value::Undefined);
    }
  };
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_to_read_fulfilled_call_id,
        state.readable_stream_pipe_to_read_rejected_call_id,
      ))
    })?;

  // Root captured values + promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(resolve)?;
  scope.push_root(reject)?;
  scope.push_root(Value::Bool(prevent_close))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeTo read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeTo read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(read_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_write_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo write rejection callback missing reader slot",
      ))
    }
  };
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);

  // Best-effort cancellation of the source stream.
  if let Ok(cancel_promise) = reader_cancel_native(vm, scope, host, hooks, callee, Value::Object(reader_obj), &[])
  {
    let _ = mark_promise_handled(scope, cancel_promise);
  }

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(reason)?;
  vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
  Ok(Value::Undefined)
}

fn readable_stream_pipe_to_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream_obj) = this else {
    return Err(VmError::TypeError("ReadableStream.pipeTo: illegal invocation"));
  };
  // Root the source stream across allocations in this helper. The VM call frame roots `this`, but we
  // also invoke other native helpers directly from Rust, which can allocate/GC before they root
  // their receivers.
  scope.push_root(Value::Object(stream_obj))?;

  // Ensure this is one of our streams (and not e.g. an arbitrary object inheriting the prototype).
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    state
      .streams
      .contains_key(&WeakGcObject::from(stream_obj))
      .then_some(())
      .ok_or(VmError::TypeError("ReadableStream.pipeTo: illegal invocation"))
  })?;

  let destination = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(destination_obj) = destination else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeTo expects a destination object",
    ));
  };

  // Acquire a writer from `destination`.
  scope.push_root(Value::Object(destination_obj))?;
  let get_writer_key = alloc_key(scope, "getWriter")?;
  let get_writer_fn =
    vm.get_with_host_and_hooks(host, scope, hooks, destination_obj, get_writer_key)?;
  if !scope.heap().is_callable(get_writer_fn)? {
    return Err(VmError::TypeError(
      "ReadableStream.pipeTo: destination.getWriter is not callable",
    ));
  }
  let writer_val = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    get_writer_fn,
    Value::Object(destination_obj),
    &[],
  )?;
  let Value::Object(writer_obj) = writer_val else {
    return Err(VmError::TypeError(
      "ReadableStream.pipeTo: getWriter did not return an object",
    ));
  };
  // Root writer across subsequent allocations (not otherwise reachable until we capture it in the
  // Promise reaction callbacks).
  scope.push_root(Value::Object(writer_obj))?;

  let mut prevent_close = false;
  let options = args.get(1).copied().unwrap_or(Value::Undefined);
  if let Value::Object(options_obj) = options {
    scope.push_root(Value::Object(options_obj))?;
    let prevent_close_key = alloc_key(scope, "preventClose")?;
    let prevent_close_val =
      vm.get_with_host_and_hooks(host, scope, hooks, options_obj, prevent_close_key)?;
    prevent_close = matches!(prevent_close_val, Value::Bool(true));
  }

  // Lock the source stream and start an asynchronous pump:
  //
  // - `source.getReader().read().then(write-to-writable, abort-on-error)`
  // - Each successful `write()` schedules the next `read()`.
  // - When `read()` completes, optionally `writer.close()` then resolve.
  let reader_val = readable_stream_get_reader_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(stream_obj),
    &[],
  )?;
  let Value::Object(reader_obj) = reader_val else {
    return Err(VmError::InvariantViolation(
      "ReadableStream.getReader must return an object",
    ));
  };
  // Root reader across subsequent allocations (not otherwise reachable until we capture it in the
  // Promise reaction callbacks).
  scope.push_root(Value::Object(reader_obj))?;

  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let read_promise = match reader_read_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  ) {
    Ok(p) => p,
    Err(err) => {
      let reason = vm_error_to_rejection_value(vm, scope, err)?;

      // Best-effort `writer.abort(reason)`; ignore failures.
      let abort_val = (|| {
        let mut scope = scope.reborrow();
        scope.push_root(Value::Object(writer_obj))?;
        scope.push_root(reason)?;
        let abort_key = alloc_key(&mut scope, "abort")?;
        let abort_fn = vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, abort_key)?;
        if !scope.heap().is_callable(abort_fn)? {
          return Ok(Value::Undefined);
        }
        vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          abort_fn,
          Value::Object(writer_obj),
          &[reason],
        )
      })();
      if let Ok(abort_val) = abort_val {
        scope.push_root(abort_val)?;
        if let Ok(p) = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, abort_val) {
          let _ = mark_promise_handled(scope, p);
        }
      }

      scope.push_root(reason)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
      return Ok(promise);
    }
  };
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_pipe_to_read_fulfilled_call_id,
        state.readable_stream_pipe_to_read_rejected_call_id,
      ))
    })?;

  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root captured values + promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(writer_obj))?;
  scope.push_root(resolve)?;
  scope.push_root(reject)?;
  scope.push_root(Value::Bool(prevent_close))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream pipeTo read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream pipeTo read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(writer_obj),
      resolve,
      reject,
      Value::Bool(prevent_close),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(read_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;
  mark_promise_handled(&mut scope, derived)?;

  Ok(promise)
}

fn readable_stream_controller_stream(
  scope: &mut Scope<'_>,
  controller: GcObject,
) -> Result<GcObject, VmError> {
  match get_data_prop(scope, controller, READABLE_STREAM_CONTROLLER_STREAM_KEY)? {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::TypeError(
      "ReadableStreamDefaultController: illegal invocation",
    )),
  }
}

fn readable_stream_controller_enqueue_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_readable_stream_controller(scope, this)?;
  let stream_obj = readable_stream_controller_stream(scope, controller_obj)?;

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  let chunk_string = match chunk {
    Value::Undefined => String::new(),
    Value::String(s) => {
      let code_units = scope.heap().get_string(s)?.as_code_units();
      let byte_len = utf8_len_from_utf16_units(code_units)?;
      if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
        return Err(VmError::TypeError("ReadableStream chunk too large"));
      }
      utf16_units_to_utf8_string_lossy(code_units, byte_len)?
    }
    _ => {
      return Err(VmError::TypeError(
        "ReadableStreamDefaultController.enqueue expects a string",
      ))
    }
  };

  let pending = enqueue_string_into_readable_stream(vm, scope, callee, stream_obj, chunk_string)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn readable_stream_controller_close_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_readable_stream_controller(scope, this)?;
  let stream_obj = readable_stream_controller_stream(scope, controller_obj)?;

  let pending = close_readable_stream(vm, scope, callee, stream_obj)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn readable_stream_controller_error_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_readable_stream_controller(scope, this)?;
  let stream_obj = readable_stream_controller_stream(scope, controller_obj)?;

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason_string = scope.heap_mut().to_string(reason)?;
  let msg = scope.heap().get_string(reason_string)?.to_utf8_lossy();

  let pending = error_readable_stream(vm, scope, callee, stream_obj, msg)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
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
  readable_stream_get_reader_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(stream_obj),
    &[],
  )
}

enum ReadChunk {
  Bytes(Vec<u8>),
  String(String),
}

enum ReadOutcome {
  Chunk(ReadChunk),
  Done,
  Error(String),
  Pending,
}

fn settle_read_promise(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  intr: &Intrinsics,
  resolve: Value,
  reject: Value,
  outcome: ReadOutcome,
) -> Result<(), VmError> {
  match outcome {
    ReadOutcome::Chunk(chunk) => match chunk {
      ReadChunk::Bytes(chunk) => {
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
      ReadChunk::String(chunk) => {
        let s = scope.alloc_string(&chunk)?;
        scope.push_root(Value::String(s))?;

        let result = scope.alloc_object()?;
        scope.push_root(Value::Object(result))?;

        let value_key = alloc_key(scope, "value")?;
        let done_key = alloc_key(scope, "done")?;

        scope.define_property(result, value_key, result_data_desc(Value::String(s)))?;
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
    },
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
      let err = new_type_error_object(scope, intr, &msg)?;
      scope.push_root(err)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[err])?;
    }
    ReadOutcome::Pending => {
      // Promise capability is stored on the reader object and will be resolved later by internal
      // enqueue/close/error operations.
    }
  }

  Ok(())
}

struct PendingReadSettle {
  reader: WeakGcObject,
  roots: Option<PendingReadRoots>,
  outcome: ReadOutcome,
}

fn take_pending_read_capability(
  scope: &mut Scope<'_>,
  reader_obj: GcObject,
) -> Result<Option<(Value, Value)>, VmError> {
  let resolve = get_data_prop(
    scope,
    reader_obj,
    READABLE_STREAM_READER_PENDING_RESOLVE_KEY,
  )?;
  let reject = get_data_prop(scope, reader_obj, READABLE_STREAM_READER_PENDING_REJECT_KEY)?;

  // Clear stored capability regardless of shape.
  set_data_prop(
    scope,
    reader_obj,
    READABLE_STREAM_READER_PENDING_RESOLVE_KEY,
    Value::Undefined,
    true,
  )?;
  set_data_prop(
    scope,
    reader_obj,
    READABLE_STREAM_READER_PENDING_REJECT_KEY,
    Value::Undefined,
    true,
  )?;

  match (resolve, reject) {
    (Value::Object(_), Value::Object(_)) => Ok(Some((resolve, reject))),
    _ => Ok(None),
  }
}

fn settle_pending_read(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  reader: WeakGcObject,
  roots: Option<PendingReadRoots>,
  outcome: ReadOutcome,
) -> Result<(), VmError> {
  let settle_result = (|| {
    let Some(reader_obj) = reader.upgrade(scope.heap()) else {
      return Ok(());
    };
    // Root the reader across property-key allocations in `take_pending_read_capability`.
    scope.push_root(Value::Object(reader_obj))?;
    let Some((resolve, reject)) = take_pending_read_capability(scope, reader_obj)? else {
      return Ok(());
    };
    let intr = require_intrinsics(vm, "ReadableStream requires intrinsics")?;
    settle_read_promise(vm, scope, host, hooks, &intr, resolve, reject, outcome)
  })();

  if let Some(roots) = roots {
    // Always remove persistent roots, even if settlement fails (to avoid leaking roots on errors).
    scope.heap_mut().remove_root(roots.reader);
    scope.heap_mut().remove_root(roots.stream);
  }

  settle_result
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
  let reader_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_DEFAULT_READER_HOST_TAG,
    "ReadableStreamDefaultReader.read: illegal invocation",
  )?;

  // Always return a Promise (spec shape), even though we resolve synchronously.
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("ReadableStream requires intrinsics"))?;

  let (outcome, pending_stream) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let reader_state = state
      .readers
      .get(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.read: illegal invocation",
      ))?;

    let Some(stream_weak) = reader_state.stream else {
      return Ok((
        ReadOutcome::Error("ReadableStreamDefaultReader has no stream (lock released)".to_string()),
        None,
      ));
    };

    let Some(stream_state) = state.streams.get_mut(&stream_weak) else {
      return Ok((
        ReadOutcome::Error("ReadableStream has been garbage collected".to_string()),
        None,
      ));
    };

    match stream_state.state {
      StreamLifecycleState::Readable => {}
      StreamLifecycleState::Closed => return Ok((ReadOutcome::Done, None)),
      StreamLifecycleState::Errored => {
        let msg = stream_state
          .error_message
          .clone()
          .unwrap_or_else(|| "ReadableStream errored".to_string());
        return Ok((ReadOutcome::Error(msg), None));
      }
    }

    if stream_state.pending_reader.is_some() {
      return Ok((
        ReadOutcome::Error("ReadableStreamDefaultReader.read: another read is pending".to_string()),
        None,
      ));
    }

    match stream_state.kind {
      StreamKind::Bytes => {
        // Lazily populate bytes from the host init closure on the first `read()`.
        if let Some(init) = stream_state.init.take() {
          match init() {
            Ok(bytes) => {
              stream_state.bytes = bytes;
              stream_state.offset = 0;
              stream_state.queue = chunk_sizes_for_len(stream_state.bytes.len());
            }
            Err(err) => {
              stream_state.state = StreamLifecycleState::Errored;
              let msg = err.to_string();
              stream_state.error_message = Some(msg.clone());
              return Ok((ReadOutcome::Error(msg), None));
            }
          }
        }

        if stream_state.queue.is_empty() {
          if stream_state.close_requested {
            stream_state.state = StreamLifecycleState::Closed;
            return Ok((ReadOutcome::Done, None));
          }

          // No bytes available yet: keep the promise pending and resolve it when bytes are enqueued.
          stream_state.pending_reader = Some(WeakGcObject::from(reader_obj));
          return Ok((ReadOutcome::Pending, Some(stream_weak)));
        }

        let Some(next_size) = stream_state.queue.pop_front() else {
          return Ok((
            ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
            None,
          ));
        };

        let chunk = if next_size == 0 {
          Vec::new()
        } else {
          let start = stream_state.offset;
          let end = start + next_size;
          let chunk = stream_state.bytes.get(start..end).unwrap_or(&[]).to_vec();
          stream_state.offset = end;
          chunk
        };

        if stream_state.close_requested && stream_state.queue.is_empty() {
          stream_state.state = StreamLifecycleState::Closed;
        }

        Ok((ReadOutcome::Chunk(ReadChunk::Bytes(chunk)), None))
      }
      StreamKind::Strings => {
        if stream_state.strings.is_empty() {
          if stream_state.close_requested {
            stream_state.state = StreamLifecycleState::Closed;
            return Ok((ReadOutcome::Done, None));
          }

          stream_state.pending_reader = Some(WeakGcObject::from(reader_obj));
          return Ok((ReadOutcome::Pending, Some(stream_weak)));
        }

        let Some(chunk) = stream_state.strings.pop_front() else {
          return Ok((
            ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
            None,
          ));
        };

        if stream_state.close_requested && stream_state.strings.is_empty() {
          stream_state.state = StreamLifecycleState::Closed;
        }

        Ok((ReadOutcome::Chunk(ReadChunk::String(chunk)), None))
      }
    }
  })?;

  if matches!(&outcome, ReadOutcome::Pending) {
    set_data_prop(
      scope,
      reader_obj,
      READABLE_STREAM_READER_PENDING_RESOLVE_KEY,
      resolve,
      true,
    )?;
    set_data_prop(
      scope,
      reader_obj,
      READABLE_STREAM_READER_PENDING_REJECT_KEY,
      reject,
      true,
    )?;

    // Root the reader + stream while a read is pending so GC can't collect the reader (which would
    // otherwise strand the pending promise capability stored on it).
    let pending_stream = pending_stream.ok_or(VmError::InvariantViolation(
      "ReadableStream pending read missing stream handle",
    ))?;
    let stream_obj = pending_stream
      .upgrade(scope.heap())
      .ok_or(VmError::InvariantViolation(
        "ReadableStream pending read stream has been garbage collected",
      ))?;

    let roots = PendingReadRoots {
      reader: scope.heap_mut().add_root(Value::Object(reader_obj))?,
      stream: scope.heap_mut().add_root(Value::Object(stream_obj))?,
    };
    if let Err(err) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
      let stream_state =
        state
          .streams
          .get_mut(&pending_stream)
          .ok_or(VmError::InvariantViolation(
            "ReadableStream pending read stream missing from registry",
          ))?;
      stream_state.pending_read_roots = Some(roots);
      Ok(())
    }) {
      // Clean up roots on failure.
      scope.heap_mut().remove_root(roots.reader);
      scope.heap_mut().remove_root(roots.stream);
      return Err(err);
    }
  }

  settle_read_promise(vm, scope, host, hooks, &intr, resolve, reject, outcome)?;

  Ok(promise)
}

fn reader_release_lock_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let reader_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_DEFAULT_READER_HOST_TAG,
    "ReadableStreamDefaultReader.releaseLock: illegal invocation",
  )?;

  let pending = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let reader_state = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.releaseLock: illegal invocation",
      ))?;

    let Some(stream_weak) = reader_state.stream.take() else {
      return Ok(None);
    };
    if let Some(stream_state) = state.streams.get_mut(&stream_weak) {
      stream_state.locked = false;
      let pending_reader = stream_state.pending_reader.take();
      if let Some(pending_reader) = pending_reader {
        return Ok(Some(PendingReadSettle {
          reader: pending_reader,
          roots: stream_state.pending_read_roots.take(),
          outcome: ReadOutcome::Error(
            "ReadableStreamDefaultReader has no stream (lock released)".to_string(),
          ),
        }));
      }
      debug_assert!(stream_state.pending_read_roots.is_none());
    }

    Ok(None)
  })?;

  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  // Drop the strong reference from the reader to the stream so the stream can be collected once
  // there are no other references.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  let stream_ref_key = alloc_key(&mut scope, READER_STREAM_REF_KEY)?;
  scope.define_property(
    reader_obj,
    stream_ref_key,
    data_desc(Value::Undefined, false),
  )?;

  Ok(Value::Undefined)
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
  let reader_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_DEFAULT_READER_HOST_TAG,
    "ReadableStreamDefaultReader.cancel: illegal invocation",
  )?;

  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("ReadableStream requires intrinsics"))?;

  let (outcome, pending_read) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let reader_state = state
      .readers
      .get_mut(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.cancel: illegal invocation",
      ))?;
    let Some(stream_weak) = reader_state.stream else {
      return Ok((ReadOutcome::Done, None));
    };
    let Some(stream_state) = state.streams.get_mut(&stream_weak) else {
      return Ok((ReadOutcome::Done, None));
    };

    if let Some(init) = stream_state.init.take() {
      let _ = init();
    }

    stream_state.state = StreamLifecycleState::Closed;
    stream_state.close_requested = true;
    stream_state.error_message = None;
    stream_state.bytes.clear();
    stream_state.queue.clear();
    stream_state.strings.clear();
    stream_state.init = None;
    stream_state.offset = 0;
    let pending_reader = stream_state.pending_reader.take();

    let pending_read = pending_reader.map(|reader| PendingReadSettle {
      reader,
      roots: stream_state.pending_read_roots.take(),
      outcome: ReadOutcome::Done,
    });
    if pending_read.is_none() {
      debug_assert!(stream_state.pending_read_roots.is_none());
    }

    Ok((ReadOutcome::Done, pending_read))
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
    ReadOutcome::Pending => {
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
    }
  }

  if let Some(pending_read) = pending_read {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending_read.reader,
      pending_read.roots,
      pending_read.outcome,
    )?;
  }

  Ok(promise)
}

fn create_readable_byte_stream_dynamic(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
) -> Result<GcObject, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.readable_stream_proto)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: READABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    state
      .streams
      .insert(WeakGcObject::from(obj), StreamState::new_empty());
    Ok(())
  })?;

  Ok(obj)
}

fn enqueue_bytes_into_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
  bytes: Vec<u8>,
) -> Result<Option<PendingReadSettle>, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

    if stream_state.kind != StreamKind::Bytes {
      return Err(VmError::TypeError(
        "ReadableStream enqueue expects byte streams",
      ));
    }

    if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
      return Err(VmError::TypeError("ReadableStream is closed"));
    }

    // Append bytes (may be empty).
    if !bytes.is_empty() {
      stream_state
        .bytes
        .try_reserve(bytes.len())
        .map_err(|_| VmError::OutOfMemory)?;
      stream_state.bytes.extend_from_slice(&bytes);
    }
    push_chunk_sizes(&mut stream_state.queue, bytes.len());

    let Some(pending_reader) = stream_state.pending_reader.take() else {
      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok(None);
    };
    let pending_roots = stream_state.pending_read_roots.take();

    let Some(next_size) = stream_state.queue.pop_front() else {
      return Ok(Some(PendingReadSettle {
        reader: pending_reader,
        roots: pending_roots,
        outcome: ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
      }));
    };

    let chunk = if next_size == 0 {
      Vec::new()
    } else {
      let start = stream_state.offset;
      let end = start + next_size;
      let chunk = stream_state.bytes.get(start..end).unwrap_or(&[]).to_vec();
      stream_state.offset = end;
      chunk
    };

    Ok(Some(PendingReadSettle {
      reader: pending_reader,
      roots: pending_roots,
      outcome: ReadOutcome::Chunk(ReadChunk::Bytes(chunk)),
    }))
  })
}

fn enqueue_string_into_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
  chunk: String,
) -> Result<Option<PendingReadSettle>, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

    if stream_state.kind != StreamKind::Strings {
      return Err(VmError::TypeError(
        "ReadableStream enqueue expects string streams",
      ));
    }

    if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
      return Err(VmError::TypeError("ReadableStream is closed"));
    }

    stream_state.strings.push_back(chunk);

    let Some(pending_reader) = stream_state.pending_reader.take() else {
      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok(None);
    };
    let pending_roots = stream_state.pending_read_roots.take();

    let Some(chunk) = stream_state.strings.pop_front() else {
      return Ok(Some(PendingReadSettle {
        reader: pending_reader,
        roots: pending_roots,
        outcome: ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
      }));
    };

    Ok(Some(PendingReadSettle {
      reader: pending_reader,
      roots: pending_roots,
      outcome: ReadOutcome::Chunk(ReadChunk::String(chunk)),
    }))
  })
}

fn close_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
) -> Result<Option<PendingReadSettle>, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream close: invalid stream"))?;

    if stream_state.state != StreamLifecycleState::Readable {
      return Ok(None);
    }

    stream_state.close_requested = true;
    let queue_is_empty = match stream_state.kind {
      StreamKind::Bytes => stream_state.queue.is_empty(),
      StreamKind::Strings => stream_state.strings.is_empty(),
    };
    if queue_is_empty {
      stream_state.state = StreamLifecycleState::Closed;
    }

    let pending_reader = stream_state.pending_reader.take();
    if let Some(reader) = pending_reader {
      let roots = stream_state.pending_read_roots.take();
      return Ok(Some(PendingReadSettle {
        reader,
        roots,
        outcome: ReadOutcome::Done,
      }));
    }

    debug_assert!(stream_state.pending_read_roots.is_none());
    Ok(None)
  })
}

fn error_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
  error_message: String,
) -> Result<Option<PendingReadSettle>, VmError> {
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream error: invalid stream"))?;

    stream_state.state = StreamLifecycleState::Errored;
    stream_state.close_requested = true;
    stream_state.error_message = Some(error_message.clone());
    stream_state.bytes.clear();
    stream_state.queue.clear();
    stream_state.strings.clear();
    stream_state.init = None;
    stream_state.offset = 0;

    let pending_reader = stream_state.pending_reader.take();
    if let Some(reader) = pending_reader {
      let roots = stream_state.pending_read_roots.take();
      return Ok(Some(PendingReadSettle {
        reader,
        roots,
        outcome: ReadOutcome::Error(error_message),
      }));
    }

    debug_assert!(stream_state.pending_read_roots.is_none());
    Ok(None)
  })
}

pub(crate) fn create_readable_byte_stream_from_bytes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  bytes: Vec<u8>,
) -> Result<GcObject, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.readable_stream_proto)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: READABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
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
  let proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.readable_stream_proto)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: READABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
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
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());

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

fn require_host_tag(
  scope: &Scope<'_>,
  this: Value,
  tag: u64,
  err: &'static str,
) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(err));
  };
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError(err))?;
  if slots.a != tag {
    return Err(VmError::TypeError(err));
  }
  Ok(obj)
}

fn require_readable_stream_controller(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  require_host_tag(
    scope,
    this,
    READABLE_STREAM_DEFAULT_CONTROLLER_HOST_TAG,
    "ReadableStreamDefaultController: illegal invocation",
  )
}

fn require_writable_stream(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  require_host_tag(
    scope,
    this,
    WRITABLE_STREAM_HOST_TAG,
    "WritableStream: illegal invocation",
  )
}

fn require_writable_stream_writer(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  require_host_tag(
    scope,
    this,
    WRITABLE_STREAM_DEFAULT_WRITER_HOST_TAG,
    "WritableStreamDefaultWriter: illegal invocation",
  )
}

fn require_transform_controller(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  require_host_tag(
    scope,
    this,
    TRANSFORM_STREAM_DEFAULT_CONTROLLER_HOST_TAG,
    "TransformStreamDefaultController: illegal invocation",
  )
}

fn require_transform_sink(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  require_host_tag(
    scope,
    this,
    TRANSFORM_STREAM_SINK_HOST_TAG,
    "TransformStream sink: illegal invocation",
  )
}

fn vm_error_to_rejection_value(
  vm: &Vm,
  scope: &mut Scope<'_>,
  err: VmError,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm, "streams require intrinsics (create a Realm first)")?;
  match err {
    VmError::Throw(value) => Ok(value),
    VmError::ThrowWithStack { value, .. } => Ok(value),
    VmError::TypeError(message) => new_type_error_object(scope, &intr, message),
    VmError::NotCallable => new_type_error_object(scope, &intr, "value is not callable"),
    VmError::NotConstructable => new_type_error_object(scope, &intr, "value is not a constructor"),
    other => Err(other),
  }
}

fn promise_reject_with_reason(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  reason: Value,
) -> Result<Value, VmError> {
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let reject = scope.push_root(cap.reject)?;
  scope.push_root(reason)?;
  vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
  Ok(promise)
}

fn mark_promise_handled(scope: &mut Scope<'_>, value: Value) -> Result<(), VmError> {
  let Value::Object(obj) = value else {
    return Ok(());
  };
  if !scope.heap().is_promise_object(obj) {
    return Ok(());
  }
  scope.heap_mut().promise_set_is_handled(obj, true)?;
  Ok(())
}

fn create_writable_stream_from_sink(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  sink_obj: GcObject,
  sink_write: Value,
  sink_close: Value,
  sink_abort: Value,
) -> Result<GcObject, VmError> {
  let proto = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.writable_stream_proto)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: WRITABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_BRAND_KEY,
    Value::Bool(true),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_KEY,
    Value::Object(sink_obj),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_WRITE_KEY,
    sink_write,
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_CLOSE_KEY,
    sink_close,
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_ABORT_KEY,
    sink_abort,
    false,
  )?;

  Ok(obj)
}

// === WritableStream ===========================================================

fn writable_stream_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "WritableStream constructor requires 'new'",
  ))
}

fn writable_stream_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm, "WritableStream requires intrinsics")?;

  // Determine instance prototype.
  let proto = {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(callee))?;
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: WRITABLE_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_BRAND_KEY,
    Value::Bool(true),
    false,
  )?;

  let sink_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let sink_obj = match sink_val {
    Value::Object(obj) => obj,
    _ => {
      let o = scope.alloc_object()?;
      scope.push_root(Value::Object(o))?;
      scope
        .heap_mut()
        .object_set_prototype(o, Some(intr.object_prototype()))?;
      o
    }
  };

  let mut sink_write = Value::Undefined;
  let mut sink_close = Value::Undefined;
  let mut sink_abort = Value::Undefined;

  if matches!(sink_val, Value::Object(_)) {
    scope.push_root(Value::Object(sink_obj))?;
    let write_key = alloc_key(scope, "write")?;
    let close_key = alloc_key(scope, "close")?;
    let abort_key = alloc_key(scope, "abort")?;

    let write_val = vm.get_with_host_and_hooks(host, scope, hooks, sink_obj, write_key)?;
    if scope.heap().is_callable(write_val)? {
      sink_write = write_val;
    }
    let close_val = vm.get_with_host_and_hooks(host, scope, hooks, sink_obj, close_key)?;
    if scope.heap().is_callable(close_val)? {
      sink_close = close_val;
    }
    let abort_val = vm.get_with_host_and_hooks(host, scope, hooks, sink_obj, abort_key)?;
    if scope.heap().is_callable(abort_val)? {
      sink_abort = abort_val;
    }
  }

  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_KEY,
    Value::Object(sink_obj),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_WRITE_KEY,
    sink_write,
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_CLOSE_KEY,
    sink_close,
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_SINK_ABORT_KEY,
    sink_abort,
    false,
  )?;

  Ok(Value::Object(obj))
}

fn writable_stream_get_writer_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let stream_obj = require_writable_stream(scope, this)?;

  let slots = scope.heap().get_function_native_slots(callee)?;
  let writer_proto = match slots
    .get(WRITABLE_STREAM_GET_WRITER_SLOT_WRITER_PROTO)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "WritableStream.getWriter missing writer prototype slot",
      ))
    }
  };

  let writer_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(writer_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_obj, Some(writer_proto))?;
  scope.heap_mut().object_set_host_slots(
    writer_obj,
    HostSlots {
      a: WRITABLE_STREAM_DEFAULT_WRITER_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(
    scope,
    writer_obj,
    WRITABLE_STREAM_WRITER_BRAND_KEY,
    Value::Bool(true),
    false,
  )?;
  set_data_prop(
    scope,
    writer_obj,
    WRITABLE_STREAM_WRITER_STREAM_KEY,
    Value::Object(stream_obj),
    false,
  )?;

  Ok(Value::Object(writer_obj))
}

fn writer_call_sink(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  sink_obj: GcObject,
  method: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let result =
    vm.call_with_host_and_hooks(host, scope, hooks, method, Value::Object(sink_obj), args);
  match result {
    Ok(value) => promise_resolve_with_host_and_hooks(vm, scope, host, hooks, value),
    Err(err) => {
      let reason = vm_error_to_rejection_value(vm, scope, err)?;
      promise_reject_with_reason(vm, scope, host, hooks, reason)
    }
  }
}

fn writer_write_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let writer_obj = require_writable_stream_writer(scope, this)?;
  let stream = match get_data_prop(scope, writer_obj, WRITABLE_STREAM_WRITER_STREAM_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };

  let sink_obj = match get_data_prop(scope, stream, WRITABLE_STREAM_SINK_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };
  let sink_write = get_data_prop(scope, stream, WRITABLE_STREAM_SINK_WRITE_KEY)?;

  if matches!(sink_write, Value::Undefined) {
    return promise_resolve_with_host_and_hooks(vm, scope, host, hooks, Value::Undefined);
  }

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  writer_call_sink(vm, scope, host, hooks, sink_obj, sink_write, &[chunk])
}

fn writer_close_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let writer_obj = require_writable_stream_writer(scope, this)?;
  let stream = match get_data_prop(scope, writer_obj, WRITABLE_STREAM_WRITER_STREAM_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };

  let sink_obj = match get_data_prop(scope, stream, WRITABLE_STREAM_SINK_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };
  let sink_close = get_data_prop(scope, stream, WRITABLE_STREAM_SINK_CLOSE_KEY)?;

  if matches!(sink_close, Value::Undefined) {
    return promise_resolve_with_host_and_hooks(vm, scope, host, hooks, Value::Undefined);
  }

  writer_call_sink(vm, scope, host, hooks, sink_obj, sink_close, &[])
}

fn writer_abort_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let writer_obj = require_writable_stream_writer(scope, this)?;
  let stream = match get_data_prop(scope, writer_obj, WRITABLE_STREAM_WRITER_STREAM_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };

  let sink_obj = match get_data_prop(scope, stream, WRITABLE_STREAM_SINK_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "WritableStreamDefaultWriter: illegal invocation",
      ))
    }
  };
  let sink_abort = get_data_prop(scope, stream, WRITABLE_STREAM_SINK_ABORT_KEY)?;

  if matches!(sink_abort, Value::Undefined) {
    return promise_resolve_with_host_and_hooks(vm, scope, host, hooks, Value::Undefined);
  }

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  writer_call_sink(vm, scope, host, hooks, sink_obj, sink_abort, &[reason])
}

// === TransformStream ==========================================================

fn transform_stream_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "TransformStream constructor requires 'new'",
  ))
}

fn transform_controller_readable_stream(
  scope: &mut Scope<'_>,
  controller: GcObject,
) -> Result<GcObject, VmError> {
  match get_data_prop(scope, controller, TRANSFORM_CONTROLLER_READABLE_STREAM_KEY)? {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::TypeError(
      "TransformStreamDefaultController: illegal invocation",
    )),
  }
}

fn transform_controller_enqueue_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_transform_controller(scope, this)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(chunk_obj) = chunk else {
    return Err(VmError::TypeError(
      "TransformStreamDefaultController.enqueue expects a Uint8Array",
    ));
  };
  if !scope.heap().is_uint8_array_object(chunk_obj) {
    return Err(VmError::TypeError(
      "TransformStreamDefaultController.enqueue expects a Uint8Array",
    ));
  }

  let bytes = scope.heap().uint8_array_data(chunk_obj)?.to_vec();
  let pending = enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_controller_error_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_transform_controller(scope, this)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason_string = scope.heap_mut().to_string(reason)?;
  let msg = scope.heap().get_string(reason_string)?.to_utf8_lossy();

  let pending = error_readable_stream(vm, scope, callee, stream_obj, msg)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_controller_terminate_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_transform_controller(scope, this)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let pending = close_readable_stream(vm, scope, callee, stream_obj)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_sink_controller(
  scope: &mut Scope<'_>,
  sink_obj: GcObject,
) -> Result<GcObject, VmError> {
  match get_data_prop(scope, sink_obj, TRANSFORM_SINK_CONTROLLER_KEY)? {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::TypeError(
      "TransformStream sink: illegal invocation",
    )),
  }
}

fn transform_sink_write_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let sink_obj = require_transform_sink(scope, this)?;

  let controller_obj = transform_sink_controller(scope, sink_obj)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let transformer = get_data_prop(scope, sink_obj, TRANSFORM_SINK_TRANSFORMER_KEY)?;
  let transform = get_data_prop(scope, sink_obj, TRANSFORM_SINK_TRANSFORM_KEY)?;

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);

  if scope.heap().is_callable(transform)? {
    let receiver = match transformer {
      Value::Object(obj) => Value::Object(obj),
      _ => Value::Undefined,
    };
    let call_res = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      transform,
      receiver,
      &[chunk, Value::Object(controller_obj)],
    );
    return match call_res {
      Ok(value) => Ok(value),
      Err(err) => {
        // Error the readable side and propagate the thrown error to the writer.
        let pending = error_readable_stream(vm, scope, callee, stream_obj, err.to_string())?;
        if let Some(pending) = pending {
          settle_pending_read(
            vm,
            scope,
            host,
            hooks,
            pending.reader,
            pending.roots,
            pending.outcome,
          )?;
        }
        Err(err)
      }
    };
  }

  // Default transform is pass-through: `controller.enqueue(chunk)`.
  let pending = match chunk {
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => {
      let bytes = scope.heap().uint8_array_data(obj)?.to_vec();
      enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?
    }
    _ => {
      return Err(VmError::TypeError(
        "TransformStream default transform expects Uint8Array chunks",
      ))
    }
  };

  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_close_after_flush_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let stream_obj = match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TransformStream close-after-flush callback missing stream slot",
      ))
    }
  };

  let pending = close_readable_stream(vm, scope, callee, stream_obj)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_close_after_flush_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);

  let slots = scope.heap().get_function_native_slots(callee)?;
  let stream_obj = match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TransformStream close-after-flush callback missing stream slot",
      ))
    }
  };

  let reason_string = scope.heap_mut().to_string(reason)?;
  let msg = scope.heap().get_string(reason_string)?.to_utf8_lossy();
  let pending = error_readable_stream(vm, scope, callee, stream_obj, msg)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Err(VmError::Throw(reason))
}

fn transform_sink_close_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let sink_obj = require_transform_sink(scope, this)?;
  let controller_obj = transform_sink_controller(scope, sink_obj)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let transformer = get_data_prop(scope, sink_obj, TRANSFORM_SINK_TRANSFORMER_KEY)?;
  let flush = get_data_prop(scope, sink_obj, TRANSFORM_SINK_FLUSH_KEY)?;

  if !scope.heap().is_callable(flush)? {
    let pending = close_readable_stream(vm, scope, callee, stream_obj)?;
    if let Some(pending) = pending {
      settle_pending_read(
        vm,
        scope,
        host,
        hooks,
        pending.reader,
        pending.roots,
        pending.outcome,
      )?;
    }
    return Ok(Value::Undefined);
  }

  let receiver = match transformer {
    Value::Object(obj) => Value::Object(obj),
    _ => Value::Undefined,
  };
  let flush_result = match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    flush,
    receiver,
    &[Value::Object(controller_obj)],
  ) {
    Ok(value) => value,
    Err(err) => {
      let pending = error_readable_stream(vm, scope, callee, stream_obj, err.to_string())?;
      if let Some(pending) = pending {
        settle_pending_read(
          vm,
          scope,
          host,
          hooks,
          pending.reader,
          pending.roots,
          pending.outcome,
        )?;
      }
      return Err(err);
    }
  };

  let flush_promise = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, flush_result)?;
  let Value::Object(flush_promise_obj) = flush_promise else {
    return Err(VmError::InvariantViolation(
      "PromiseResolve must return an object",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.transform_close_after_flush_fulfilled_call_id,
        state.transform_close_after_flush_rejected_call_id,
      ))
    })?;

  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root stream and promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(stream_obj))?;
  scope.push_root(flush_promise)?;

  let on_fulfilled_name = scope.alloc_string("TransformStream flush fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[realm_slot, Value::Object(stream_obj)],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("TransformStream flush rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[realm_slot, Value::Object(stream_obj)],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let derived = perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(flush_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )?;

  Ok(derived)
}

fn transform_sink_abort_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let sink_obj = require_transform_sink(scope, this)?;
  let controller_obj = transform_sink_controller(scope, sink_obj)?;
  let stream_obj = transform_controller_readable_stream(scope, controller_obj)?;

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason_string = scope.heap_mut().to_string(reason)?;
  let msg = scope.heap().get_string(reason_string)?.to_utf8_lossy();

  let pending = error_readable_stream(vm, scope, callee, stream_obj, msg)?;
  if let Some(pending) = pending {
    settle_pending_read(
      vm,
      scope,
      host,
      hooks,
      pending.reader,
      pending.roots,
      pending.outcome,
    )?;
  }

  Ok(Value::Undefined)
}

fn transform_stream_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm, "TransformStream requires intrinsics")?;

  // Determine instance prototype.
  let proto = {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(callee))?;
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => intr.object_prototype(),
    }
  };

  let ts_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(ts_obj))?;
  scope.heap_mut().object_set_prototype(ts_obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    ts_obj,
    HostSlots {
      a: TRANSFORM_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  // Allocate `readable` and the internal controller for enqueuing.
  let readable = create_readable_byte_stream_dynamic(vm, scope, callee)?;
  scope.push_root(Value::Object(readable))?;

  let (controller_proto, sink_write, sink_close, sink_abort) = {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let controller_proto = match slots
      .get(TRANSFORM_STREAM_CTOR_SLOT_CONTROLLER_PROTO)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TransformStream constructor missing controller prototype slot",
        ))
      }
    };

    let sink_write = match slots
      .get(TRANSFORM_STREAM_CTOR_SLOT_SINK_WRITE_FN)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => Value::Object(obj),
      _ => {
        return Err(VmError::InvariantViolation(
          "TransformStream constructor missing sink write function slot",
        ))
      }
    };
    let sink_close = match slots
      .get(TRANSFORM_STREAM_CTOR_SLOT_SINK_CLOSE_FN)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => Value::Object(obj),
      _ => {
        return Err(VmError::InvariantViolation(
          "TransformStream constructor missing sink close function slot",
        ))
      }
    };
    let sink_abort = match slots
      .get(TRANSFORM_STREAM_CTOR_SLOT_SINK_ABORT_FN)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => Value::Object(obj),
      _ => {
        return Err(VmError::InvariantViolation(
          "TransformStream constructor missing sink abort function slot",
        ))
      }
    };

    (controller_proto, sink_write, sink_close, sink_abort)
  };

  let controller_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(controller_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_obj, Some(controller_proto))?;
  scope.heap_mut().object_set_host_slots(
    controller_obj,
    HostSlots {
      a: TRANSFORM_STREAM_DEFAULT_CONTROLLER_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(
    scope,
    controller_obj,
    TRANSFORM_CONTROLLER_BRAND_KEY,
    Value::Bool(true),
    false,
  )?;
  set_data_prop(
    scope,
    controller_obj,
    TRANSFORM_CONTROLLER_READABLE_STREAM_KEY,
    Value::Object(readable),
    false,
  )?;

  // Parse transformer `{ transform, flush }`.
  let transformer_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut transformer_obj = Value::Undefined;
  let mut transform_fn = Value::Undefined;
  let mut flush_fn = Value::Undefined;

  if let Value::Object(obj) = transformer_val {
    transformer_obj = Value::Object(obj);

    scope.push_root(transformer_val)?;
    let transform_key = alloc_key(scope, "transform")?;
    let flush_key = alloc_key(scope, "flush")?;
    let t = vm.get_with_host_and_hooks(host, scope, hooks, obj, transform_key)?;
    if scope.heap().is_callable(t)? {
      transform_fn = t;
    }
    let f = vm.get_with_host_and_hooks(host, scope, hooks, obj, flush_key)?;
    if scope.heap().is_callable(f)? {
      flush_fn = f;
    }
  }

  // Create the internal sink object for the writable side.
  let sink_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(sink_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(sink_obj, Some(intr.object_prototype()))?;
  scope.heap_mut().object_set_host_slots(
    sink_obj,
    HostSlots {
      a: TRANSFORM_STREAM_SINK_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(
    scope,
    sink_obj,
    TRANSFORM_SINK_BRAND_KEY,
    Value::Bool(true),
    false,
  )?;
  set_data_prop(
    scope,
    sink_obj,
    TRANSFORM_SINK_TRANSFORMER_KEY,
    transformer_obj,
    false,
  )?;
  set_data_prop(
    scope,
    sink_obj,
    TRANSFORM_SINK_TRANSFORM_KEY,
    transform_fn,
    false,
  )?;
  set_data_prop(scope, sink_obj, TRANSFORM_SINK_FLUSH_KEY, flush_fn, false)?;
  set_data_prop(
    scope,
    sink_obj,
    TRANSFORM_SINK_CONTROLLER_KEY,
    Value::Object(controller_obj),
    false,
  )?;

  // Attach sink methods to the object so `WritableStreamDefaultWriter` can call them.
  let write_key = alloc_key(scope, "write")?;
  let close_key = alloc_key(scope, "close")?;
  let abort_key = alloc_key(scope, "abort")?;
  scope.define_property(sink_obj, write_key, data_desc(sink_write, true))?;
  scope.define_property(sink_obj, close_key, data_desc(sink_close, true))?;
  scope.define_property(sink_obj, abort_key, data_desc(sink_abort, true))?;

  let writable = create_writable_stream_from_sink(
    vm, scope, callee, sink_obj, sink_write, sink_close, sink_abort,
  )?;
  scope.push_root(Value::Object(writable))?;

  let readable_key = alloc_key(scope, "readable")?;
  let writable_key = alloc_key(scope, "writable")?;
  scope.define_property(
    ts_obj,
    readable_key,
    read_only_data_desc(Value::Object(readable)),
  )?;
  scope.define_property(
    ts_obj,
    writable_key,
    read_only_data_desc(Value::Object(writable)),
  )?;

  Ok(Value::Object(ts_obj))
}

pub(crate) fn readable_stream_is_locked(vm: &Vm, heap: &Heap, obj: GcObject) -> bool {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());

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
    return state.streams.get(&key).map_or(false, |s| s.locked);
  }

  // If we don't have a current realm (e.g. tests calling native handlers directly), fall back to
  // scanning all installed realm states. The number of realms is expected to be small.
  for state in registry.realms.values_mut() {
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      state.streams.retain(|k, _| k.upgrade(heap).is_some());
      state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }
    if let Some(stream_state) = state.streams.get(&key) {
      return stream_state.locked;
    }
  }

  false
}

pub fn install_window_streams_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let realm_id = realm.id();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- ReadableStream ----------------------------------------------------------
  let stream_call_id: NativeFunctionId = vm.register_native_call(readable_stream_ctor_call)?;
  let stream_construct_id: NativeConstructId =
    vm.register_native_construct(readable_stream_ctor_construct)?;

  let stream_name = scope.alloc_string("ReadableStream")?;
  scope.push_root(Value::String(stream_name))?;
  let stream_ctor = scope.alloc_native_function_with_slots(
    stream_call_id,
    Some(stream_construct_id),
    stream_name,
    1,
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

  let get_reader_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_get_reader_native)?;
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
  scope.define_property(
    stream_proto,
    get_reader_key,
    data_desc(Value::Object(get_reader_fn), true),
  )?;

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
  scope.define_property(
    stream_proto,
    cancel_key,
    data_desc(Value::Object(cancel_fn), true),
  )?;

  let pipe_through_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_through_native)?;
  let pipe_through_name = scope.alloc_string("pipeThrough")?;
  scope.push_root(Value::String(pipe_through_name))?;
  let pipe_through_fn = scope.alloc_native_function_with_slots(
    pipe_through_call_id,
    None,
    pipe_through_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(pipe_through_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(pipe_through_fn, Some(intr.function_prototype()))?;
  let pipe_through_key = alloc_key(&mut scope, "pipeThrough")?;
  scope.define_property(
    stream_proto,
    pipe_through_key,
    data_desc(Value::Object(pipe_through_fn), true),
  )?;

  let pipe_to_call_id: NativeFunctionId = vm.register_native_call(readable_stream_pipe_to_native)?;
  let pipe_to_name = scope.alloc_string("pipeTo")?;
  scope.push_root(Value::String(pipe_to_name))?;
  let pipe_to_fn = scope.alloc_native_function_with_slots(
    pipe_to_call_id,
    None,
    pipe_to_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(pipe_to_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(pipe_to_fn, Some(intr.function_prototype()))?;
  let pipe_to_key = alloc_key(&mut scope, "pipeTo")?;
  scope.define_property(
    stream_proto,
    pipe_to_key,
    data_desc(Value::Object(pipe_to_fn), true),
  )?;

  let locked_get_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_locked_get_native)?;
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
  scope.define_property(
    stream_proto,
    stream_tag_key,
    data_desc(Value::String(stream_tag_val), false),
  )?;

  let stream_ctor_key = alloc_key(&mut scope, "ReadableStream")?;
  scope.define_property(
    global,
    stream_ctor_key,
    data_desc(Value::Object(stream_ctor), true),
  )?;

  // pipeThrough pump callbacks.
  let readable_stream_pipe_through_read_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_through_read_fulfilled_native)?;
  let readable_stream_pipe_through_read_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_through_read_rejected_native)?;
  let readable_stream_pipe_through_write_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_through_write_fulfilled_native)?;
  let readable_stream_pipe_through_write_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_through_write_rejected_native)?;

  // pipeTo pump callbacks.
  let readable_stream_pipe_to_read_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_read_fulfilled_native)?;
  let readable_stream_pipe_to_read_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_read_rejected_native)?;
  let readable_stream_pipe_to_write_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_write_fulfilled_native)?;
  let readable_stream_pipe_to_write_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_write_rejected_native)?;
  let readable_stream_pipe_to_close_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_close_fulfilled_native)?;
  let readable_stream_pipe_to_close_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_pipe_to_close_rejected_native)?;

  // --- ReadableStreamDefaultController (prototype only; no global constructor) ------------------
  let readable_controller_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(readable_controller_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(readable_controller_proto, Some(intr.object_prototype()))?;

  let controller_enqueue_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_controller_enqueue_native)?;
  let controller_enqueue_name = scope.alloc_string("enqueue")?;
  scope.push_root(Value::String(controller_enqueue_name))?;
  let controller_enqueue_fn = scope.alloc_native_function_with_slots(
    controller_enqueue_call_id,
    None,
    controller_enqueue_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_enqueue_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_enqueue_fn, Some(intr.function_prototype()))?;
  let controller_enqueue_key = alloc_key(&mut scope, "enqueue")?;
  scope.define_property(
    readable_controller_proto,
    controller_enqueue_key,
    data_desc(Value::Object(controller_enqueue_fn), true),
  )?;

  let controller_close_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_controller_close_native)?;
  let controller_close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(controller_close_name))?;
  let controller_close_fn = scope.alloc_native_function_with_slots(
    controller_close_call_id,
    None,
    controller_close_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_close_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_close_fn, Some(intr.function_prototype()))?;
  let controller_close_key = alloc_key(&mut scope, "close")?;
  scope.define_property(
    readable_controller_proto,
    controller_close_key,
    data_desc(Value::Object(controller_close_fn), true),
  )?;

  let controller_error_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_controller_error_native)?;
  let controller_error_name = scope.alloc_string("error")?;
  scope.push_root(Value::String(controller_error_name))?;
  let controller_error_fn = scope.alloc_native_function_with_slots(
    controller_error_call_id,
    None,
    controller_error_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_error_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_error_fn, Some(intr.function_prototype()))?;
  let controller_error_key = alloc_key(&mut scope, "error")?;
  scope.define_property(
    readable_controller_proto,
    controller_error_key,
    data_desc(Value::Object(controller_error_fn), true),
  )?;

  // --- ReadableStreamDefaultReader --------------------------------------------
  let reader_call_id: NativeFunctionId = vm.register_native_call(reader_ctor_call)?;
  let reader_construct_id: NativeConstructId =
    vm.register_native_construct(reader_ctor_construct)?;

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
  scope.define_property(
    reader_proto,
    read_key,
    data_desc(Value::Object(read_fn), true),
  )?;

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
  scope.define_property(
    reader_proto,
    release_key,
    data_desc(Value::Object(release_fn), true),
  )?;

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
  scope.define_property(
    reader_proto,
    reader_tag_key,
    data_desc(Value::String(reader_tag_val), false),
  )?;

  let reader_ctor_key = alloc_key(&mut scope, "ReadableStreamDefaultReader")?;
  scope.define_property(
    global,
    reader_ctor_key,
    data_desc(Value::Object(reader_ctor), true),
  )?;

  // --- WritableStream ---------------------------------------------------------
  let writable_stream_call_id: NativeFunctionId =
    vm.register_native_call(writable_stream_ctor_call)?;
  let writable_stream_construct_id: NativeConstructId =
    vm.register_native_construct(writable_stream_ctor_construct)?;

  let writable_stream_name = scope.alloc_string("WritableStream")?;
  scope.push_root(Value::String(writable_stream_name))?;
  let writable_stream_ctor = scope.alloc_native_function_with_slots(
    writable_stream_call_id,
    Some(writable_stream_construct_id),
    writable_stream_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(writable_stream_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(writable_stream_ctor, Some(intr.function_prototype()))?;

  let writable_stream_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(writable_stream_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "WritableStream constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(writable_stream_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(writable_stream_proto, Some(intr.object_prototype()))?;

  // WritableStreamDefaultWriter prototype (not exposed as a global constructor).
  let writer_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(writer_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_proto, Some(intr.object_prototype()))?;

  let writer_write_call_id: NativeFunctionId = vm.register_native_call(writer_write_native)?;
  let writer_write_name = scope.alloc_string("write")?;
  scope.push_root(Value::String(writer_write_name))?;
  let writer_write_fn =
    scope.alloc_native_function(writer_write_call_id, None, writer_write_name, 1)?;
  scope.push_root(Value::Object(writer_write_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_write_fn, Some(intr.function_prototype()))?;
  let writer_write_key = alloc_key(&mut scope, "write")?;
  scope.define_property(
    writer_proto,
    writer_write_key,
    data_desc(Value::Object(writer_write_fn), true),
  )?;

  let writer_close_call_id: NativeFunctionId = vm.register_native_call(writer_close_native)?;
  let writer_close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(writer_close_name))?;
  let writer_close_fn =
    scope.alloc_native_function(writer_close_call_id, None, writer_close_name, 0)?;
  scope.push_root(Value::Object(writer_close_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_close_fn, Some(intr.function_prototype()))?;
  let writer_close_key = alloc_key(&mut scope, "close")?;
  scope.define_property(
    writer_proto,
    writer_close_key,
    data_desc(Value::Object(writer_close_fn), true),
  )?;

  let writer_abort_call_id: NativeFunctionId = vm.register_native_call(writer_abort_native)?;
  let writer_abort_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(writer_abort_name))?;
  let writer_abort_fn =
    scope.alloc_native_function(writer_abort_call_id, None, writer_abort_name, 1)?;
  scope.push_root(Value::Object(writer_abort_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_abort_fn, Some(intr.function_prototype()))?;
  let writer_abort_key = alloc_key(&mut scope, "abort")?;
  scope.define_property(
    writer_proto,
    writer_abort_key,
    data_desc(Value::Object(writer_abort_fn), true),
  )?;

  let writer_tag_key = PropertyKey::from_symbol(to_string_tag);
  let writer_tag_val = scope.alloc_string("WritableStreamDefaultWriter")?;
  scope.push_root(Value::String(writer_tag_val))?;
  scope.define_property(
    writer_proto,
    writer_tag_key,
    data_desc(Value::String(writer_tag_val), false),
  )?;

  let get_writer_call_id: NativeFunctionId =
    vm.register_native_call(writable_stream_get_writer_native)?;
  let get_writer_name = scope.alloc_string("getWriter")?;
  scope.push_root(Value::String(get_writer_name))?;
  let get_writer_fn = scope.alloc_native_function_with_slots(
    get_writer_call_id,
    None,
    get_writer_name,
    0,
    &[
      Value::Number(realm_id.to_raw() as f64),
      Value::Object(writer_proto),
    ],
  )?;
  scope.push_root(Value::Object(get_writer_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(get_writer_fn, Some(intr.function_prototype()))?;
  let get_writer_key = alloc_key(&mut scope, "getWriter")?;
  scope.define_property(
    writable_stream_proto,
    get_writer_key,
    data_desc(Value::Object(get_writer_fn), true),
  )?;

  let writable_stream_tag_key = PropertyKey::from_symbol(to_string_tag);
  let writable_stream_tag_val = scope.alloc_string("WritableStream")?;
  scope.push_root(Value::String(writable_stream_tag_val))?;
  scope.define_property(
    writable_stream_proto,
    writable_stream_tag_key,
    data_desc(Value::String(writable_stream_tag_val), false),
  )?;

  let writable_stream_ctor_key = alloc_key(&mut scope, "WritableStream")?;
  scope.define_property(
    global,
    writable_stream_ctor_key,
    data_desc(Value::Object(writable_stream_ctor), true),
  )?;

  // --- TransformStream --------------------------------------------------------
  // TransformStreamDefaultController prototype (not exposed as a global constructor).
  let controller_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(controller_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_proto, Some(intr.object_prototype()))?;

  let controller_enqueue_call_id: NativeFunctionId =
    vm.register_native_call(transform_controller_enqueue_native)?;
  let controller_enqueue_name = scope.alloc_string("enqueue")?;
  scope.push_root(Value::String(controller_enqueue_name))?;
  let controller_enqueue_fn = scope.alloc_native_function_with_slots(
    controller_enqueue_call_id,
    None,
    controller_enqueue_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_enqueue_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_enqueue_fn, Some(intr.function_prototype()))?;
  let controller_enqueue_key = alloc_key(&mut scope, "enqueue")?;
  scope.define_property(
    controller_proto,
    controller_enqueue_key,
    data_desc(Value::Object(controller_enqueue_fn), true),
  )?;

  let controller_error_call_id: NativeFunctionId =
    vm.register_native_call(transform_controller_error_native)?;
  let controller_error_name = scope.alloc_string("error")?;
  scope.push_root(Value::String(controller_error_name))?;
  let controller_error_fn = scope.alloc_native_function_with_slots(
    controller_error_call_id,
    None,
    controller_error_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_error_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_error_fn, Some(intr.function_prototype()))?;
  let controller_error_key = alloc_key(&mut scope, "error")?;
  scope.define_property(
    controller_proto,
    controller_error_key,
    data_desc(Value::Object(controller_error_fn), true),
  )?;

  let controller_terminate_call_id: NativeFunctionId =
    vm.register_native_call(transform_controller_terminate_native)?;
  let controller_terminate_name = scope.alloc_string("terminate")?;
  scope.push_root(Value::String(controller_terminate_name))?;
  let controller_terminate_fn = scope.alloc_native_function_with_slots(
    controller_terminate_call_id,
    None,
    controller_terminate_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_terminate_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(controller_terminate_fn, Some(intr.function_prototype()))?;
  let controller_terminate_key = alloc_key(&mut scope, "terminate")?;
  scope.define_property(
    controller_proto,
    controller_terminate_key,
    data_desc(Value::Object(controller_terminate_fn), true),
  )?;

  let sink_write_call_id: NativeFunctionId =
    vm.register_native_call(transform_sink_write_native)?;
  let sink_write_name = scope.alloc_string("write")?;
  scope.push_root(Value::String(sink_write_name))?;
  let sink_write_fn = scope.alloc_native_function_with_slots(
    sink_write_call_id,
    None,
    sink_write_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(sink_write_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(sink_write_fn, Some(intr.function_prototype()))?;

  let sink_close_call_id: NativeFunctionId =
    vm.register_native_call(transform_sink_close_native)?;
  let sink_close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(sink_close_name))?;
  let sink_close_fn = scope.alloc_native_function_with_slots(
    sink_close_call_id,
    None,
    sink_close_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(sink_close_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(sink_close_fn, Some(intr.function_prototype()))?;

  let sink_abort_call_id: NativeFunctionId =
    vm.register_native_call(transform_sink_abort_native)?;
  let sink_abort_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(sink_abort_name))?;
  let sink_abort_fn = scope.alloc_native_function_with_slots(
    sink_abort_call_id,
    None,
    sink_abort_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(sink_abort_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(sink_abort_fn, Some(intr.function_prototype()))?;

  let transform_close_after_flush_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(transform_close_after_flush_fulfilled_native)?;
  let transform_close_after_flush_rejected_call_id: NativeFunctionId =
    vm.register_native_call(transform_close_after_flush_rejected_native)?;

  let ts_call_id: NativeFunctionId = vm.register_native_call(transform_stream_ctor_call)?;
  let ts_construct_id: NativeConstructId =
    vm.register_native_construct(transform_stream_ctor_construct)?;

  let ts_name = scope.alloc_string("TransformStream")?;
  scope.push_root(Value::String(ts_name))?;
  let ts_ctor = scope.alloc_native_function_with_slots(
    ts_call_id,
    Some(ts_construct_id),
    ts_name,
    1,
    &[
      Value::Number(realm_id.to_raw() as f64),
      Value::Object(sink_write_fn),
      Value::Object(sink_close_fn),
      Value::Object(sink_abort_fn),
      Value::Object(controller_proto),
    ],
  )?;
  scope.push_root(Value::Object(ts_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ts_ctor, Some(intr.function_prototype()))?;

  let ts_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ts_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TransformStream constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(ts_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(ts_proto, Some(intr.object_prototype()))?;

  let ts_tag_key = PropertyKey::from_symbol(to_string_tag);
  let ts_tag_val = scope.alloc_string("TransformStream")?;
  scope.push_root(Value::String(ts_tag_val))?;
  scope.define_property(
    ts_proto,
    ts_tag_key,
    data_desc(Value::String(ts_tag_val), false),
  )?;

  let ts_ctor_key = alloc_key(&mut scope, "TransformStream")?;
  scope.define_property(global, ts_ctor_key, data_desc(Value::Object(ts_ctor), true))?;

  // Register per-realm state.
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
  registry.realms.insert(
    realm_id,
    StreamRealmState {
      readable_stream_proto: stream_proto,
      readable_stream_controller_proto: readable_controller_proto,
      reader_proto,
      writable_stream_proto,
      writer_proto,
      transform_stream_proto: ts_proto,
      transform_controller_proto: controller_proto,
      transform_close_after_flush_fulfilled_call_id,
      transform_close_after_flush_rejected_call_id,
      readable_stream_pipe_through_read_fulfilled_call_id,
      readable_stream_pipe_through_read_rejected_call_id,
      readable_stream_pipe_through_write_fulfilled_call_id,
      readable_stream_pipe_through_write_rejected_call_id,
      readable_stream_pipe_to_read_fulfilled_call_id,
      readable_stream_pipe_to_read_rejected_call_id,
      readable_stream_pipe_to_write_fulfilled_call_id,
      readable_stream_pipe_to_write_rejected_call_id,
      readable_stream_pipe_to_close_fulfilled_call_id,
      readable_stream_pipe_to_close_rejected_call_id,
      streams: HashMap::new(),
      readers: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub fn teardown_window_streams_bindings_for_realm(realm_id: RealmId) {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());
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
  fn readable_stream_underlying_source_start_controller_supports_enqueue_and_close(
  ) -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.source = {
          start(controller) {
            globalThis.controller = controller;
            globalThis.startThisOk = this === globalThis.source;
          }
        };
        globalThis.stream = new ReadableStream(globalThis.source);
      "#,
    )?;

    assert_eq!(realm.exec_script("startThisOk")?, Value::Bool(true));
    let enqueue_ty = realm.exec_script("typeof controller.enqueue")?;
    assert_eq!(get_string(realm.heap(), enqueue_ty), "function");
    let close_ty = realm.exec_script("typeof controller.close")?;
    assert_eq!(get_string(realm.heap(), close_ty), "function");
    let error_ty = realm.exec_script("typeof controller.error")?;
    assert_eq!(get_string(realm.heap(), error_ty), "function");

    let _ = realm.exec_script("globalThis.reader = stream.getReader();")?;

    let read_p = realm.exec_script("globalThis.readPromise = reader.read();")?;
    let Value::Object(read_p_obj) = read_p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("controller.enqueue('hi');")?;

    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result_val) = realm.heap().promise_result(read_p_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result_obj, "value")?;
      assert_eq!(get_string(scope.heap(), value), "hi");
    }

    // `undefined` chunks are treated as empty strings (matches `TextEncoder.encode(undefined)`).
    let read_p2 = realm.exec_script("globalThis.readPromise2 = reader.read();")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p2_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("controller.enqueue();")?;

    assert_eq!(
      realm.heap().promise_state(read_p2_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result2_val) = realm.heap().promise_result(read_p2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result2_obj) = result2_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result2_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result2_obj, "value")?;
      assert_eq!(get_string(scope.heap(), value), "");
    }

    let _ = realm.exec_script("controller.close();")?;

    let read_p3 = realm.exec_script("globalThis.readPromise3 = reader.read();")?;
    let Value::Object(read_p3_obj) = read_p3 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p3_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result3_val) = realm.heap().promise_result(read_p3_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result3_obj) = result3_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result3_obj, "done")?;
      assert_eq!(done, Value::Bool(true));
      let value = read_result_prop(&mut scope, result3_obj, "value")?;
      assert!(matches!(value, Value::Undefined));
    }

    let enqueue_after_close_threw =
      realm.exec_script("(() => { try { controller.enqueue('x'); return false; } catch (e) { return e instanceof TypeError; } })()")?;
    assert_eq!(enqueue_after_close_threw, Value::Bool(true));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_pipe_through_text_encoder_stream_encodes_string_chunks() -> Result<(), VmError>
  {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.piped = new ReadableStream({
          start(controller) { globalThis.controller = controller; }
        }).pipeThrough(new TextEncoderStream());
        globalThis.reader = piped.getReader();
        globalThis.readPromise1 = reader.read();
      "#,
    )?;

    let read_p1 = realm.exec_script("readPromise1")?;
    let Value::Object(read_p1_obj) = read_p1 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p1_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("controller.enqueue('hi');")?;

    assert_eq!(
      realm.heap().promise_state(read_p1_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result_val) = realm.heap().promise_result(read_p1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result_obj, "value")?;
      let Value::Object(value_obj) = value else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      assert!(scope.heap().is_uint8_array_object(value_obj));
      assert_eq!(scope.heap().uint8_array_data(value_obj)?, b"hi");
    }

    let read_p2 = realm.exec_script("globalThis.readPromise2 = reader.read();")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p2_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("controller.close();")?;

    assert_eq!(
      realm.heap().promise_state(read_p2_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result2_val) = realm.heap().promise_result(read_p2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result2_obj) = result2_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };
    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result2_obj, "done")?;
      assert_eq!(done, Value::Bool(true));
      let value = read_result_prop(&mut scope, result2_obj, "value")?;
      assert!(matches!(value, Value::Undefined));
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_pending_read_survives_gc_without_reader_reference() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // `stream.getReader().read()` without retaining the reader object is a common JS pattern. We
    // must still be able to resolve the returned promise when data is enqueued, even after GC.
    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) { globalThis.controller = controller; }
        });
        globalThis.readPromise = stream.getReader().read();
      "#,
    )?;

    let read_p = realm.exec_script("readPromise")?;
    let Value::Object(read_p_obj) = read_p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );

    // Force GC: the reader object is not referenced from JS, so without host-side rooting this
    // would strand the pending promise.
    realm.heap_mut().collect_garbage();

    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );
    let _ = realm.exec_script("controller.enqueue('ok');")?;

    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result_val) = realm.heap().promise_result(read_p_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };
    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result_obj, "value")?;
      assert_eq!(get_string(scope.heap(), value), "ok");
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_underlying_source_controller_error_rejects_reads() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) {
            globalThis.controller = controller;
          }
        });
        globalThis.reader = stream.getReader();
        globalThis.readPromise = reader.read();
      "#,
    )?;

    let read_p = realm.exec_script("readPromise")?;
    let Value::Object(read_p_obj) = read_p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("controller.error('boom');")?;
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Rejected
    );

    let read_p2 = realm.exec_script("reader.read()")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p2_obj)?,
      PromiseState::Rejected
    );

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

      let stream =
        create_readable_byte_stream_from_bytes(vm, &mut scope, ctor_obj, b"hi".to_vec())?;
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
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done1 = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done1, Value::Bool(false));
      let value1 = read_result_prop(&mut scope, result1_obj, "value")?;
      let Value::Object(value1_obj) = value1 else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
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
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
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

  #[test]
  fn writable_stream_is_installed_and_writer_write_resolves() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ty = realm.exec_script("typeof WritableStream")?;
    assert_eq!(get_string(realm.heap(), ty), "function");

    let p = realm.exec_script("new WritableStream().getWriter().write('x')")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "WritableStreamDefaultWriter.write must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Fulfilled);

    realm.teardown();
    Ok(())
  }

  #[test]
  fn transform_stream_can_enqueue_into_readable() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let ty = realm.exec_script("typeof TransformStream")?;
    assert_eq!(get_string(realm.heap(), ty), "function");

    let _ = realm.exec_script(
      "globalThis.ts = new TransformStream({ transform(chunk, controller) { controller.enqueue(chunk); } });",
    )?;
    let _ = realm.exec_script("globalThis.reader = ts.readable.getReader();")?;
    let read_p = realm.exec_script("globalThis.readPromise = reader.read();")?;
    let Value::Object(read_p_obj) = read_p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("globalThis.writer = ts.writable.getWriter();")?;
    let write_p =
      realm.exec_script("globalThis.writePromise = writer.write(new Uint8Array([1,2,3]));")?;
    let Value::Object(write_p_obj) = write_p else {
      return Err(VmError::InvariantViolation(
        "WritableStreamDefaultWriter.write must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(write_p_obj)?,
      PromiseState::Fulfilled
    );

    // Enqueue should have resolved the pending read.
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result_val) = realm.heap().promise_result(read_p_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result_obj, "value")?;
      let Value::Object(value_obj) = value else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      assert!(scope.heap().is_uint8_array_object(value_obj));
      assert_eq!(scope.heap().uint8_array_data(value_obj)?, &[1, 2, 3]);
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn text_encoder_stream_can_encode_into_readable() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script("globalThis.tes = new TextEncoderStream();")?;
    let _ = realm.exec_script("globalThis.reader = tes.readable.getReader();")?;
    let read_p = realm.exec_script("globalThis.readPromise = reader.read();")?;
    let Value::Object(read_p_obj) = read_p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Pending
    );

    let _ = realm.exec_script("globalThis.writer = tes.writable.getWriter();")?;
    let write_p = realm.exec_script("globalThis.writePromise = writer.write('hi');")?;
    let Value::Object(write_p_obj) = write_p else {
      return Err(VmError::InvariantViolation(
        "WritableStreamDefaultWriter.write must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(write_p_obj)?,
      PromiseState::Fulfilled
    );

    assert_eq!(
      realm.heap().promise_state(read_p_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result_val) = realm.heap().promise_result(read_p_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done = read_result_prop(&mut scope, result_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result_obj, "value")?;
      let Value::Object(value_obj) = value else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      assert!(scope.heap().is_uint8_array_object(value_obj));
      assert_eq!(scope.heap().uint8_array_data(value_obj)?, b"hi");
    }

    realm.teardown();
    Ok(())
  }
}
