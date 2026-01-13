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
//! - full tee/backpressure (this implementation provides `ReadableStream.prototype.tee()` for byte
//!   and string streams only)
//!
//! The goal is to provide just enough surface area for real-world code that expects streams
//! constructors to exist and for host-owned byte sources to be consumed via
//! `readable.getReader().read()`.

use std::char;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use vm_js::{
  new_promise_capability_with_host_and_hooks, new_range_error, new_type_error_object,
  perform_promise_then_with_host_and_hooks, promise_resolve_with_host_and_hooks, GcObject, Heap,
  HostSlots, Intrinsics, NativeConstructId, NativeFunctionId, PromiseCapability, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
  WeakGcObject,
};

const STREAM_REALM_ID_SLOT: usize = 0;
const READER_STREAM_REF_KEY: &str = "__fastrender_readable_stream_reader_stream_ref";

// Brand stream wrappers as platform objects via HostSlots so structuredClone rejects them with
// DataCloneError (streams are not structured-cloneable without special transfer support).
const READABLE_STREAM_HOST_TAG: u64 = 0x5245_4144_5354_524D; // "READSTRM"
const READABLE_STREAM_DEFAULT_READER_HOST_TAG: u64 = 0x5253_5245_4144_4552; // "RSREADER"
const READABLE_STREAM_DEFAULT_CONTROLLER_HOST_TAG: u64 = 0x5253_434E_5452_4C52; // "RSCNTRLR"
const READABLE_STREAM_ASYNC_ITERATOR_HOST_TAG: u64 = 0x5253_4153_594E_4349; // "RSASYNCI"
const WRITABLE_STREAM_HOST_TAG: u64 = 0x5752_4954_5354_524D; // "WRITSTRM"
const WRITABLE_STREAM_DEFAULT_WRITER_HOST_TAG: u64 = 0x5753_5752_4954_4552; // "WSWRITER"
const TRANSFORM_STREAM_HOST_TAG: u64 = 0x5452_4E53_5354_524D; // "TRNSSTRM"
const TRANSFORM_STREAM_DEFAULT_CONTROLLER_HOST_TAG: u64 = 0x5453_434E_5452_4C52; // "TSCNTRLR"
const TRANSFORM_STREAM_SINK_HOST_TAG: u64 = 0x5453_5349_4E4B_5F5F; // "TSSINK__"
// QueuingStrategy interface objects are also platform objects and are not structured-cloneable.
const BYTE_LENGTH_QUEUING_STRATEGY_HOST_TAG: u64 = 0x424C_5153_5452_4154; // "BLQSTRAT"
const COUNT_QUEUING_STRATEGY_HOST_TAG: u64 = 0x434E_5451_5354_5259; // "CNTQSTRY"

// Hidden per-iterator own properties.
const ITER_READER_KEY: &str = "__fastrender_readable_stream_iter_reader";
const ITER_PREVENT_CANCEL_KEY: &str = "__fastrender_readable_stream_iter_prevent_cancel";

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

/// Upper bound for a single byte chunk enqueued via a `ReadableStream` controller.
///
/// This mirrors `MAX_READABLE_STREAM_STRING_CHUNK_BYTES`; scripts can enqueue attacker-controlled
/// `Uint8Array` / `ArrayBuffer` chunks, so we cap the per-chunk host allocation.
const MAX_READABLE_STREAM_BYTE_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// Default `highWaterMark` used by `ReadableStreamDefaultController`.
///
/// This matches the spec default for default controllers.
const DEFAULT_READABLE_STREAM_HIGH_WATER_MARK: f64 = 1.0;

/// Hard upper bound on total buffered content per `ReadableStream`.
///
/// This is a DoS resistance measure: even with per-chunk caps, hostile code can enqueue unbounded
/// numbers of small chunks (or `tee()` can duplicate buffered data), otherwise leading to OOM.
///
/// Note: this implementation does not implement full backpressure/high-water-mark semantics, so
/// this limit acts as a simple safety valve.
const MAX_READABLE_STREAM_QUEUED_BYTES: usize = 32 * 1024 * 1024; // 32MiB

/// Hard upper bound on number of queued items for object-mode (`ReadableStream` of arbitrary JS
/// values).
const MAX_READABLE_STREAM_QUEUED_ITEMS: usize = 100_000;

/// Hard upper bound on number of queued chunks for byte/string streams.
///
/// Without this, hostile code can enqueue extremely small (or empty) chunks to grow the queue's
/// metadata (VecDeque element storage) without exceeding `MAX_READABLE_STREAM_QUEUED_BYTES`.
const MAX_READABLE_STREAM_QUEUED_CHUNKS: usize = 100_000;

const READABLE_STREAM_QUEUE_LIMIT_ERROR: &str = "ReadableStream queue size limit exceeded";

// --- Hidden, internal property keys -----------------------------------------------------------

const READABLE_STREAM_READER_PENDING_RESOLVE_KEY: &str =
  "__fastrender_readable_stream_pending_read_resolve";
const READABLE_STREAM_READER_PENDING_REJECT_KEY: &str =
  "__fastrender_readable_stream_pending_read_reject";
const READABLE_STREAM_STORED_ERROR_KEY: &str = "__fastrender_readable_stream_stored_error";

const READABLE_STREAM_CONTROLLER_BRAND_KEY: &str =
  "__fastrender_readable_stream_default_controller";
const READABLE_STREAM_CONTROLLER_STREAM_KEY: &str =
  "__fastrender_readable_stream_default_controller_stream";

const WRITABLE_STREAM_BRAND_KEY: &str = "__fastrender_writable_stream";
const WRITABLE_STREAM_SINK_KEY: &str = "__fastrender_writable_stream_sink";
const WRITABLE_STREAM_SINK_WRITE_KEY: &str = "__fastrender_writable_stream_sink_write";
const WRITABLE_STREAM_SINK_CLOSE_KEY: &str = "__fastrender_writable_stream_sink_close";
const WRITABLE_STREAM_SINK_ABORT_KEY: &str = "__fastrender_writable_stream_sink_abort";
const WRITABLE_STREAM_LOCKED_KEY: &str = "__fastrender_writable_stream_locked";

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

const READABLE_STREAM_TEE_SLOT_READER: usize = 1;
const READABLE_STREAM_TEE_SLOT_BRANCH0: usize = 2;
const READABLE_STREAM_TEE_SLOT_BRANCH1: usize = 3;

const READABLE_STREAM_START_REJECTED_SLOT_STREAM: usize = 1;

const READABLE_STREAM_TEE_BRANCH_CANCEL_SLOT_READER: usize = 1;
const READABLE_STREAM_TEE_BRANCH_CANCEL_SLOT_OTHER_BRANCH: usize = 2;

fn push_byte_chunks(
  queue: &mut VecDeque<Vec<u8>>,
  buffered_len: &mut usize,
  bytes: Vec<u8>,
) -> Result<(), VmError> {
  // For dynamic streams, enqueue boundaries must be preserved even for empty chunks.
  if bytes.is_empty() {
    queue
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    queue.push_back(Vec::new());
    return Ok(());
  }

  let len = bytes.len();
  let new_buffered_len = buffered_len.checked_add(len).ok_or(VmError::OutOfMemory)?;

  if len <= STREAM_CHUNK_BYTES {
    queue
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    queue.push_back(bytes);
    *buffered_len = new_buffered_len;
    return Ok(());
  }

  let chunk_count = len
    .checked_add(STREAM_CHUNK_BYTES - 1)
    .ok_or(VmError::OutOfMemory)?
    / STREAM_CHUNK_BYTES;
  let mut new_chunks: VecDeque<Vec<u8>> = VecDeque::new();
  new_chunks
    .try_reserve(chunk_count)
    .map_err(|_| VmError::OutOfMemory)?;

  let mut offset = 0;
  while offset < len {
    let end = (offset + STREAM_CHUNK_BYTES).min(len);
    let slice = &bytes[offset..end];
    let mut chunk = Vec::new();
    chunk
      .try_reserve_exact(slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    chunk.extend_from_slice(slice);
    new_chunks.push_back(chunk);
    offset = end;
  }

  // Reserve on the destination queue only after chunk allocation succeeds, so we don't partially
  // mutate the stream state on allocation failure.
  queue
    .try_reserve(chunk_count)
    .map_err(|_| VmError::OutOfMemory)?;
  queue.append(&mut new_chunks);
  *buffered_len = new_buffered_len;

  Ok(())
}

fn chunk_bytes(bytes: Vec<u8>) -> Result<(VecDeque<Vec<u8>>, usize), VmError> {
  // Fixed streams treat a zero-length buffer as immediately closed, not as an empty chunk.
  if bytes.is_empty() {
    return Ok((VecDeque::new(), 0));
  }
  let mut queue = VecDeque::new();
  let mut buffered_len = 0;
  // `push_byte_chunks` enqueues empty chunks for empty buffers, but we already handled the empty
  // case above.
  push_byte_chunks(&mut queue, &mut buffered_len, bytes)?;
  Ok((queue, buffered_len))
}

fn extract_buffer_source_bytes(
  scope: &mut Scope<'_>,
  chunk: Value,
) -> Result<Option<Vec<u8>>, VmError> {
  let heap = scope.heap();
  let Value::Object(chunk_obj) = chunk else {
    return Ok(None);
  };

  if heap.is_uint8_array_object(chunk_obj) {
    let data = heap.uint8_array_data(chunk_obj)?;
    if data.len() > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(data.to_vec()));
  }

  if heap.is_typed_array_object(chunk_obj) {
    let (buffer_obj, byte_offset, byte_len) = heap.typed_array_view_bytes(chunk_obj)?;
    if byte_len > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    let data = heap.array_buffer_data(buffer_obj)?;
    let end = byte_offset.checked_add(byte_len).ok_or(VmError::InvariantViolation(
      "TypedArray byte offset overflow while enqueuing ReadableStream chunk",
    ))?;
    let slice = data.get(byte_offset..end).ok_or(VmError::InvariantViolation(
      "TypedArray view out of bounds while enqueuing ReadableStream chunk",
    ))?;
    return Ok(Some(slice.to_vec()));
  }

  if heap.is_data_view_object(chunk_obj) {
    let buffer_obj = heap.data_view_buffer(chunk_obj)?;
    let byte_offset = heap.data_view_byte_offset(chunk_obj)?;
    let byte_len = heap.data_view_byte_length(chunk_obj)?;
    if byte_len > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    let data = heap.array_buffer_data(buffer_obj)?;
    let end = byte_offset.checked_add(byte_len).ok_or(VmError::InvariantViolation(
      "DataView byte offset overflow while enqueuing ReadableStream chunk",
    ))?;
    let slice = data.get(byte_offset..end).ok_or(VmError::InvariantViolation(
      "DataView view out of bounds while enqueuing ReadableStream chunk",
    ))?;
    return Ok(Some(slice.to_vec()));
  }

  if heap.is_array_buffer_object(chunk_obj) {
    let data = heap.array_buffer_data(chunk_obj)?;
    if data.len() > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(data.to_vec()));
  }

  Ok(None)
}

fn buffer_source_byte_length(scope: &Scope<'_>, chunk: Value) -> Result<Option<usize>, VmError> {
  let heap = scope.heap();
  let Value::Object(chunk_obj) = chunk else {
    return Ok(None);
  };

  if heap.is_uint8_array_object(chunk_obj) {
    let data = heap.uint8_array_data(chunk_obj)?;
    if data.len() > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(data.len()));
  }

  if heap.is_typed_array_object(chunk_obj) {
    let (_buffer_obj, _byte_offset, byte_len) = heap.typed_array_view_bytes(chunk_obj)?;
    if byte_len > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(byte_len));
  }

  if heap.is_data_view_object(chunk_obj) {
    let byte_len = heap.data_view_byte_length(chunk_obj)?;
    if byte_len > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(byte_len));
  }

  if heap.is_array_buffer_object(chunk_obj) {
    let data = heap.array_buffer_data(chunk_obj)?;
    if data.len() > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
      return Err(VmError::TypeError("ReadableStream chunk too large"));
    }
    return Ok(Some(data.len()));
  }

  Ok(None)
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
  /// Stream chunks are arbitrary JS values (object-mode).
  ///
  /// Queued values are stored as persistent GC roots and delivered verbatim via
  /// `ReadableStreamDefaultReader.read()`.
  Values,
  /// Stream kind is determined by the first controller `enqueue` call.
  ///
  /// This is used for JS-constructed streams created via `new ReadableStream({ start(...) { ... } })`
  /// so they can become either a byte stream (`Uint8Array`/`ArrayBuffer` chunks) or a string stream
  /// (string chunks).
  Uninitialized,
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
  high_water_mark: f64,
  /// `true` if no more bytes will be enqueued into the stream.
  ///
  /// For fixed-byte streams (e.g. `create_readable_byte_stream_from_bytes`), this is `true` from
  /// creation.
  ///
  /// For dynamic streams (e.g. `new ReadableStream()` or `TransformStream.readable`), this is set
  /// by internal close/terminate paths.
  close_requested: bool,
  error_message: Option<String>,
  /// Byte chunks remaining in the stream.
  ///
  /// For dynamic streams (`TransformStream.readable`), this preserves enqueue boundaries (after
  /// internal `STREAM_CHUNK_BYTES` chunking) and ensures empty chunks (`Uint8Array(0)`) still
  /// resolve pending reads.
  byte_queue: VecDeque<Vec<u8>>,
  /// Total number of bytes currently buffered across `byte_queue`.
  buffered_byte_len: usize,
  /// Queue of string chunks (only used when `kind == StreamKind::Strings`).
  strings: VecDeque<String>,
  /// Total number of UTF-8 bytes currently buffered across `strings`.
  buffered_string_len: usize,
  /// Queue of rooted JS values (only used when `kind == StreamKind::Values`).
  values: VecDeque<RootId>,
  init: Option<LazyInit>,
  /// A pending `reader.read()` call waiting for bytes (only used for dynamic streams).
  pending_reader: Option<WeakGcObject>,
  pending_read_roots: Option<PendingReadRoots>,
  kind: StreamKind,
}

impl StreamState {
  fn new_empty(high_water_mark: f64) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      high_water_mark,
      close_requested: false,
      error_message: None,
      byte_queue: VecDeque::new(),
      buffered_byte_len: 0,
      strings: VecDeque::new(),
      buffered_string_len: 0,
      values: VecDeque::new(),
      init: None,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Bytes,
    }
  }

  fn new_empty_strings(high_water_mark: f64) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      high_water_mark,
      close_requested: false,
      error_message: None,
      byte_queue: VecDeque::new(),
      buffered_byte_len: 0,
      strings: VecDeque::new(),
      buffered_string_len: 0,
      values: VecDeque::new(),
      init: None,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Strings,
    }
  }

  fn new_empty_uninitialized(high_water_mark: f64) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      high_water_mark,
      close_requested: false,
      error_message: None,
      byte_queue: VecDeque::new(),
      buffered_byte_len: 0,
      strings: VecDeque::new(),
      buffered_string_len: 0,
      values: VecDeque::new(),
      init: None,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Uninitialized,
    }
  }

  fn new_from_bytes(bytes: Vec<u8>, high_water_mark: f64) -> Result<Self, VmError> {
    if bytes.len() > MAX_READABLE_STREAM_QUEUED_BYTES {
      return Err(VmError::TypeError(READABLE_STREAM_QUEUE_LIMIT_ERROR));
    }
    let chunk_count = if bytes.is_empty() {
      0
    } else if bytes.len() <= STREAM_CHUNK_BYTES {
      1
    } else {
      bytes
        .len()
        .checked_add(STREAM_CHUNK_BYTES - 1)
        .ok_or(VmError::OutOfMemory)?
        / STREAM_CHUNK_BYTES
    };
    if chunk_count > MAX_READABLE_STREAM_QUEUED_CHUNKS {
      return Err(VmError::TypeError(READABLE_STREAM_QUEUE_LIMIT_ERROR));
    }

    let (byte_queue, buffered_byte_len) = chunk_bytes(bytes)?;
    Ok(Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      high_water_mark,
      close_requested: true,
      error_message: None,
      byte_queue,
      buffered_byte_len,
      strings: VecDeque::new(),
      buffered_string_len: 0,
      values: VecDeque::new(),
      init: None,
      pending_reader: None,
      pending_read_roots: None,
      kind: StreamKind::Bytes,
    })
  }

  fn new_lazy(init: LazyInit, high_water_mark: f64) -> Self {
    Self {
      locked: false,
      state: StreamLifecycleState::Readable,
      high_water_mark,
      close_requested: true,
      error_message: None,
      byte_queue: VecDeque::new(),
      buffered_byte_len: 0,
      strings: VecDeque::new(),
      buffered_string_len: 0,
      values: VecDeque::new(),
      init: Some(init),
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
  readable_stream_start_rejected_call_id: NativeFunctionId,
  readable_stream_tee_read_fulfilled_call_id: NativeFunctionId,
  readable_stream_tee_read_rejected_call_id: NativeFunctionId,
  readable_stream_tee_branch_cancel_call_id: NativeFunctionId,
  iterator_next_id: NativeFunctionId,
  iterator_next_fulfilled_id: NativeFunctionId,
  iterator_next_rejected_id: NativeFunctionId,
  iterator_return_id: NativeFunctionId,
  iterator_async_iterator_id: NativeFunctionId,
  streams: HashMap<WeakGcObject, StreamState>,
  readers: HashMap<WeakGcObject, ReaderState>,
  /// Roots extracted from streams that were swept while we only had an immutable `&Heap`.
  ///
  /// These are removed opportunistically the next time we enter `with_realm_state_mut` with a
  /// mutable heap borrow.
  pending_value_root_cleanup: Vec<RootId>,
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

fn get_data_prop_opt(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(scope.heap().object_get_own_data_property_value(obj, &key)?)
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
  vm: &Vm,
  scope: &mut Scope<'_>,
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

  // Drain any deferred root cleanup work (see `StreamRealmState::pending_value_root_cleanup`).
  if !state.pending_value_root_cleanup.is_empty() {
    for root_id in state.pending_value_root_cleanup.drain(..) {
      scope.heap_mut().remove_root(root_id);
    }
  }

  // Opportunistically sweep dead objects when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    // Identify dead streams using an immutable heap borrow first...
    let dead_streams: Vec<WeakGcObject> = {
      let heap = scope.heap();
      state
        .streams
        .keys()
        .copied()
        .filter(|k| k.upgrade(heap).is_none())
        .collect()
    };

    // ...then remove them and clean up any persistent roots they own.
    for k in dead_streams {
      let Some(mut stream_state) = state.streams.remove(&k) else {
        continue;
      };
      for root_id in stream_state.values.drain(..) {
        scope.heap_mut().remove_root(root_id);
      }
    }

    let heap = scope.heap();
    state.readers.retain(|k, _| k.upgrade(heap).is_some());
  }

  f(state, scope.heap())
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

  // Parse strategy `{ highWaterMark }` (minimal).
  let mut high_water_mark: f64 = DEFAULT_READABLE_STREAM_HIGH_WATER_MARK;
  let strategy_val = args.get(1).copied().unwrap_or(Value::Undefined);
  if !matches!(strategy_val, Value::Undefined | Value::Null) {
    let strategy_obj = scope.to_object(vm, host, hooks, strategy_val)?;
    scope.push_root(Value::Object(strategy_obj))?;
    let high_water_mark_key = alloc_key(scope, "highWaterMark")?;
    let high_water_mark_val =
      vm.get_with_host_and_hooks(host, scope, hooks, strategy_obj, high_water_mark_key)?;
    if !matches!(high_water_mark_val, Value::Undefined) {
      let mut parsed = scope.heap_mut().to_number(high_water_mark_val)?;
      if parsed.is_nan() || parsed < 0.0 {
        let intr = require_intrinsics(vm, "ReadableStream requires intrinsics")?;
        return Err(VmError::Throw(new_range_error(
          scope,
          intr,
          "The highWaterMark value is invalid.",
        )?));
      }
      // Canonicalize -0 to +0.
      if parsed == 0.0 {
        parsed = 0.0;
      }
      high_water_mark = parsed;
    }
  }

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
        StreamState::new_empty_uninitialized(high_water_mark)
      } else {
        StreamState::new_empty(high_water_mark)
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

    let start_result = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      start_fn,
      receiver,
      &[Value::Object(controller_obj)],
    );
    match start_result {
      Ok(start_val) => {
        // If `start()` returned a promise/thenable, ensure rejections error the stream (spec shape)
        // instead of surfacing as unhandled rejections / leaving the stream readable.
        //
        // Note: `PromiseResolve` can throw (e.g. if a Promise `constructor` getter throws), and
        // `PerformPromiseThen` can throw (Promise species side effects). Per WHATWG Streams, we
        // treat these as start-algorithm failures and error the stream without throwing from the
        // constructor.
        let attach_result: Result<(), VmError> = (|| {
          let start_promise = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, start_val)?;
          let Value::Object(promise_obj) = start_promise else {
            return Ok(());
          };
          if !scope.heap().is_promise_object(promise_obj) {
            return Ok(());
          }

          let rejected_call_id = with_realm_state_mut(vm, scope, callee, |state, _heap| {
            Ok(state.readable_stream_start_rejected_call_id)
          })?;
          let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
          let realm_slot = Value::Number(realm_id.to_raw() as f64);

          // Root captured values + promise across callback allocation.
          let mut scope = scope.reborrow();
          scope.push_root(Value::Object(obj))?;
          scope.push_root(start_promise)?;
          scope.push_root(realm_slot)?;

          let on_rejected_name = scope.alloc_string("ReadableStream start rejected")?;
          scope.push_root(Value::String(on_rejected_name))?;
          let on_rejected = scope.alloc_native_function_with_slots(
            rejected_call_id,
            None,
            on_rejected_name,
            1,
            &[realm_slot, Value::Object(obj)],
          )?;
          scope.push_root(Value::Object(on_rejected))?;

          let derived = perform_promise_then_with_host_and_hooks(
            vm,
            &mut scope,
            host,
            hooks,
            start_promise,
            None,
            Some(Value::Object(on_rejected)),
          )?;
          // The derived promise is internal; mark it handled so internal failures do not trigger
          // `unhandledrejection`.
          mark_promise_handled(&mut scope, derived)?;
          Ok(())
        })();

        if let Err(err) = attach_result {
          let msg: String = match err {
            VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => {
              scope.push_root(reason)?;
              match scope.heap_mut().to_string(reason) {
                Ok(reason_string) => match scope.heap().get_string(reason_string) {
                  Ok(s) => s.to_utf8_lossy(),
                  Err(_) => "ReadableStream start threw".to_string(),
                },
                Err(_) => "ReadableStream start threw".to_string(),
              }
            }
            VmError::TypeError(message) => message.to_string(),
            VmError::NotCallable => "value is not callable".to_string(),
            VmError::NotConstructable => "value is not a constructor".to_string(),
            other => return Err(other),
          };

          let pending = error_readable_stream(vm, scope, callee, obj, msg)?;
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
        }
      }
      Err(err) => {
        // Per WHATWG Streams `SetUpReadableStreamDefaultController`, exceptions thrown by the
        // `startAlgorithm` should error the stream, but not throw from the `ReadableStream`
        // constructor.
        let msg: String = match err {
          VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => {
            // Root the thrown value across allocations in `to_string`.
            scope.push_root(reason)?;
            match scope.heap_mut().to_string(reason) {
              Ok(reason_string) => match scope.heap().get_string(reason_string) {
                Ok(s) => s.to_utf8_lossy(),
                Err(_) => "ReadableStream start threw".to_string(),
              },
              Err(_) => "ReadableStream start threw".to_string(),
            }
          }
          VmError::TypeError(message) => message.to_string(),
          VmError::NotCallable => "value is not callable".to_string(),
          VmError::NotConstructable => "value is not a constructor".to_string(),
          other => return Err(other),
        };

        let pending = error_readable_stream(vm, scope, callee, obj, msg)?;
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
      }
    }
  }

  Ok(Value::Object(obj))
}

fn readable_stream_start_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let stream_obj = match slots
    .get(READABLE_STREAM_START_REJECTED_SLOT_STREAM)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream start rejected callback missing stream slot",
      ))
    }
  };

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  // The rejection reason can be any JS value. Converting it to a string may throw (e.g. Symbols, or
  // custom objects with throwing `toString`). Since this callback is internal plumbing, we must not
  // let those errors escape; otherwise the stream would remain readable and pending reads would
  // hang forever.
  let msg = match scope.heap_mut().to_string(reason) {
    Ok(reason_string) => match scope.heap().get_string(reason_string) {
      Ok(s) => s.to_utf8_lossy(),
      Err(_) => "ReadableStream start rejected".to_string(),
    },
    Err(err) if err.is_throw_completion() => "ReadableStream start rejected".to_string(),
    Err(err) => return Err(err),
  };

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

fn readable_stream_get_reader_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let stream_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_HOST_TAG,
    "ReadableStream.getReader: illegal invocation",
  )?;

  // WebIDL `ReadableStreamGetReaderOptions` dictionary:
  //
  // `ReadableStream.prototype.getReader(optional ReadableStreamGetReaderOptions options = {})`.
  //
  // Dictionary semantics:
  // - `undefined`/`null` => treat as empty dictionary.
  // - Otherwise => `ToObject(options)` and read `options.mode`.
  //
  // The only supported mode value is `"byob"`, which would request a BYOB reader. BYOB readers are
  // currently not implemented, so explicitly throw a TypeError to avoid silently returning a
  // default reader (which can break consumers relying on BYOB semantics).
  let options = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(options, Value::Undefined | Value::Null) {
    // Root the options object across property access (which can allocate/GC).
    let options_obj = scope.to_object(vm, host, hooks, options)?;
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(options_obj))?;

    let mode_key = alloc_key(&mut scope, "mode")?;
    let mode_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, mode_key)?;

    // If `mode` is present/defined, it must be `"byob"` (the only valid enum value).
    if !matches!(mode_val, Value::Undefined) {
      // Root `mode_val` across `ToString` conversion.
      scope.push_root(mode_val)?;
      let mode_s = scope.to_string(vm, host, hooks, mode_val)?;
      let mode = scope.heap().get_string(mode_s)?.to_utf8_lossy();
      if mode == "byob" {
        return Err(VmError::TypeError(
          "ReadableStream.getReader: BYOB readers are not supported",
        ));
      }
      return Err(VmError::TypeError(
        "ReadableStream.getReader: invalid mode",
      ));
    }
  }

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
  enum CancelAction {
    Resolve { value_roots: Vec<RootId> },
    Reject { error_message: Option<String> },
  }

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
  let reject = scope.push_root(cap.reject)?;

  // Determine the cancel behavior from the stream state. For errored streams we must reject with
  // the stored error reason (and MUST NOT close/reset the stream), even if the stream is locked.
  let cancel_action = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError(
        "ReadableStream.cancel: illegal invocation",
      ))?;

    match stream_state.state {
      StreamLifecycleState::Closed => return Ok(CancelAction::Resolve { value_roots: Vec::new() }),
      StreamLifecycleState::Errored => {
        return Ok(CancelAction::Reject {
          error_message: stream_state.error_message.clone(),
        })
      }
      StreamLifecycleState::Readable => {}
    }

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
    stream_state.byte_queue.clear();
    stream_state.buffered_byte_len = 0;
    stream_state.strings.clear();
    stream_state.buffered_string_len = 0;
    let value_roots: Vec<RootId> = stream_state.values.drain(..).collect();
    stream_state.init = None;
    stream_state.pending_reader = None;
    stream_state.pending_read_roots = None;
    Ok(CancelAction::Resolve { value_roots })
  })?;

  match cancel_action {
    CancelAction::Resolve { value_roots } => {
      for root_id in value_roots {
        scope.heap_mut().remove_root(root_id);
      }
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
    }
    CancelAction::Reject { error_message } => {
      // Prefer the stored JS error reason (set by controller.error or internal error paths).
      let reason = match get_data_prop_opt(scope, stream_obj, READABLE_STREAM_STORED_ERROR_KEY)? {
        Some(v) => v,
        None => match error_message {
          Some(msg) => Value::String(scope.alloc_string(&msg)?),
          None => Value::Undefined,
        },
      };
      scope.push_root(reason)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
    }
  };

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

fn resolve_async_iterator_done_promise(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
) -> Result<Value, VmError> {
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;

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
  Ok(promise)
}

fn readable_stream_values_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let stream_obj = require_host_tag(
    scope,
    this,
    READABLE_STREAM_HOST_TAG,
    "ReadableStream.values: illegal invocation",
  )?;

  // Minimal ReadableStreamIteratorOptions parsing: { preventCancel }.
  let prevent_cancel = match args.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Undefined | Value::Null => false,
    Value::Object(options) => {
      // Root options while allocating the property key: `alloc_key` can trigger GC.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(options))?;
      let key = alloc_key(&mut scope, "preventCancel")?;
      let v = vm.get_with_host_and_hooks(host, &mut scope, hooks, options, key)?;
      scope.heap().to_boolean(v)?
    }
    _ => false,
  };

  // Acquire a default reader.
  let reader_val =
    readable_stream_get_reader_native(vm, scope, host, hooks, callee, Value::Object(stream_obj), &[])?;
  let Value::Object(reader_obj) = reader_val else {
    return Err(VmError::InvariantViolation(
      "ReadableStream.getReader must return an object",
    ));
  };

  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let (next_id, return_id, async_iter_id) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok((
      state.iterator_next_id,
      state.iterator_return_id,
      state.iterator_async_iterator_id,
    ))
  })?;

  let intr = require_intrinsics(vm, "ReadableStream requires intrinsics")?;
  let func_proto = intr.function_prototype();

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  scope
    .heap_mut()
    .object_set_prototype(iter, Some(intr.object_prototype()))?;
  scope.heap_mut().object_set_host_slots(
    iter,
    HostSlots {
      a: READABLE_STREAM_ASYNC_ITERATOR_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(scope, iter, ITER_READER_KEY, Value::Object(reader_obj), false)?;
  set_data_prop(
    scope,
    iter,
    ITER_PREVENT_CANCEL_KEY,
    Value::Bool(prevent_cancel),
    false,
  )?;

  let next_name = scope.alloc_string("next")?;
  scope.push_root(Value::String(next_name))?;
  let next_fn = scope.alloc_native_function_with_slots(
    next_id,
    None,
    next_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(next_fn, Some(func_proto))?;
  set_data_prop(scope, iter, "next", Value::Object(next_fn), true)?;

  let return_name = scope.alloc_string("return")?;
  scope.push_root(Value::String(return_name))?;
  let return_fn = scope.alloc_native_function_with_slots(
    return_id,
    None,
    return_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(return_fn, Some(func_proto))?;
  set_data_prop(scope, iter, "return", Value::Object(return_fn), true)?;

  let async_iter_name = scope.alloc_string("[Symbol.asyncIterator]")?;
  scope.push_root(Value::String(async_iter_name))?;
  let async_iter_fn = scope.alloc_native_function_with_slots(
    async_iter_id,
    None,
    async_iter_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(async_iter_fn, Some(func_proto))?;
  scope.push_root(Value::Object(async_iter_fn))?;
  let async_iter_key = PropertyKey::from_symbol(intr.well_known_symbols().async_iterator);
  scope.define_property(iter, async_iter_key, data_desc(Value::Object(async_iter_fn), true))?;

  Ok(Value::Object(iter))
}

fn iterator_next_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamAsyncIterator.next: illegal invocation",
    ));
  };

  let reader_val = get_data_prop(scope, iter_obj, ITER_READER_KEY)?;
  let Value::Object(reader_obj) = reader_val else {
    return resolve_async_iterator_done_promise(vm, scope, host, hooks);
  };

  let read_promise =
    reader_read_native(vm, scope, host, hooks, callee, Value::Object(reader_obj), &[])?;
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return a Promise",
    ));
  };

  let (fulfilled_id, rejected_id) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok((state.iterator_next_fulfilled_id, state.iterator_next_rejected_id))
  })?;
  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root captured values across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(iter_obj))?;
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream async iterator next fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(iter_obj),
      Value::Object(reader_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream async iterator next rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(iter_obj),
      Value::Object(reader_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    Value::Object(read_promise_obj),
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )
}

fn iterator_next_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let result = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(result_obj) = result else {
    return Ok(result);
  };

  // Root result object while reading `done`.
  scope.push_root(Value::Object(result_obj))?;
  let done_key = alloc_key(scope, "done")?;
  let done_val = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, done_key)?;
  let done = scope.heap().to_boolean(done_val)?;

  if !done {
    return Ok(result);
  }

  let slots = scope.heap().get_function_native_slots(callee)?;
  let iter_obj = match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream async iterator next callback missing iterator slot",
      ))
    }
  };
  let reader_obj = match slots.get(2).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream async iterator next callback missing reader slot",
      ))
    }
  };

  // Auto-release lock on normal completion so `for await...of` doesn't leave the stream locked.
  let _ = reader_release_lock_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  );
  let _ = set_data_prop(scope, iter_obj, ITER_READER_KEY, Value::Undefined, false);

  Ok(result)
}

fn iterator_next_rejected_native(
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
  let iter_obj = match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("ReadableStream iterator callback missing iterator slot")),
  };
  let reader_obj = match slots.get(2).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("ReadableStream iterator callback missing reader slot")),
  };

  let _ = reader_release_lock_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  );
  let _ = set_data_prop(scope, iter_obj, ITER_READER_KEY, Value::Undefined, false);

  Err(VmError::Throw(reason))
}

fn iterator_return_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamAsyncIterator.return: illegal invocation",
    ));
  };

  let prevent_cancel = match get_data_prop(scope, iter_obj, ITER_PREVENT_CANCEL_KEY)? {
    Value::Bool(b) => b,
    _ => false,
  };

  if let Value::Object(reader_obj) = get_data_prop(scope, iter_obj, ITER_READER_KEY)? {
    if !prevent_cancel {
      let _ = reader_cancel_native(vm, scope, host, hooks, callee, Value::Object(reader_obj), &[]);
    }
    let _ =
      reader_release_lock_native(vm, scope, host, hooks, callee, Value::Object(reader_obj), &[]);
    set_data_prop(scope, iter_obj, ITER_READER_KEY, Value::Undefined, false)?;
  }

  resolve_async_iterator_done_promise(vm, scope, host, hooks)
}

fn iterator_async_iterator_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(this)
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

  // pipeThrough pipes the current stream to `transform.writable` and returns `transform.readable`.
  // It intentionally ignores the returned Promise from pipeTo, but must still mark it as handled
  // so internal piping failures don't trigger `unhandledrejection`.
  scope.push_root(Value::Object(writable_obj))?;
  scope.push_root(Value::Object(readable_obj))?;

  let pipe_to_key = alloc_key(scope, "pipeTo")?;
  let pipe_to_fn = vm.get_with_host_and_hooks(host, scope, hooks, stream_obj, pipe_to_key)?;
  if !scope.heap().is_callable(pipe_to_fn)? {
    return Err(VmError::TypeError(
      "ReadableStream.pipeThrough: this.pipeTo is not callable",
    ));
  }

  let options = args.get(1).copied().unwrap_or(Value::Undefined);
  scope.push_root(options)?;
  let promise = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    pipe_to_fn,
    Value::Object(stream_obj),
    &[Value::Object(writable_obj), options],
  )?;
  let _ = mark_promise_handled(scope, promise);

  Ok(Value::Object(readable_obj))
}

fn pipe_to_best_effort_release_writer_lock(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  writer_obj: GcObject,
) {
  let _ = (|| -> Result<(), VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(writer_obj))?;
    let release_lock_key = alloc_key(&mut scope, "releaseLock")?;
    let release_lock_fn =
      vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, release_lock_key)?;
    if !scope.heap().is_callable(release_lock_fn)? {
      return Ok(());
    }
    let _ = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      hooks,
      release_lock_fn,
      Value::Object(writer_obj),
      &[],
    )?;
    Ok(())
  })();
}

fn pipe_to_best_effort_release_locks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  reader_obj: GcObject,
  writer_obj: GcObject,
) {
  let _ = reader_release_lock_native(
    vm,
    scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  );
  pipe_to_best_effort_release_writer_lock(vm, scope, host, hooks, writer_obj);
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
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo close callback missing reader slot",
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
        "ReadableStream pipeTo close callback missing writer slot",
      ))
    }
  };
  let resolve = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_RESOLVE)
    .copied()
    .unwrap_or(Value::Undefined);
  vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
  pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo close rejection callback missing reader slot",
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
        "ReadableStream pipeTo close rejection callback missing writer slot",
      ))
    }
  };
  let reject = slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_REJECT)
    .copied()
    .unwrap_or(Value::Undefined);
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(reason)?;
  vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
  pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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
      pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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
        pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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
      pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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

  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root reader, writer, and promise across callback allocation.
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
  let reader_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo read rejection callback missing reader slot",
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
  pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);

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
  let writer_obj = match slots
    .get(READABLE_STREAM_PIPE_TO_SLOT_WRITER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream pipeTo write rejection callback missing writer slot",
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
  pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
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

  // Return a Promise immediately and reject it on any synchronous error (e.g. bad destination).
  let cap: PromiseCapability = new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = scope.push_root(cap.promise)?;
  let resolve = scope.push_root(cap.resolve)?;
  let reject = scope.push_root(cap.reject)?;

  let mut pipe_to_cleanup_reader: Option<GcObject> = None;
  let mut pipe_to_cleanup_writer: Option<GcObject> = None;

  let start_pump_result: Result<(), VmError> = (|| {
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
    pipe_to_cleanup_writer = Some(writer_obj);
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
      prevent_close = scope.heap().to_boolean(prevent_close_val)?;
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
    pipe_to_cleanup_reader = Some(reader_obj);
    // Root reader across subsequent allocations (not otherwise reachable until we capture it in the
    // Promise reaction callbacks).
    scope.push_root(Value::Object(reader_obj))?;

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
          let abort_fn =
            vm.get_with_host_and_hooks(host, &mut scope, hooks, writer_obj, abort_key)?;
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
        pipe_to_best_effort_release_locks(vm, scope, host, hooks, callee, reader_obj, writer_obj);
        return Ok(());
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

    Ok(())
  })();

  if let Err(err) = start_pump_result {
    let reason = vm_error_to_rejection_value(vm, scope, err)?;
    scope.push_root(reason)?;
    vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
    if let Some(reader_obj) = pipe_to_cleanup_reader {
      let _ = reader_release_lock_native(
        vm,
        scope,
        host,
        hooks,
        callee,
        Value::Object(reader_obj),
        &[],
      );
    }
    if let Some(writer_obj) = pipe_to_cleanup_writer {
      pipe_to_best_effort_release_writer_lock(vm, scope, host, hooks, writer_obj);
    }
  }

  Ok(promise)
}

fn readable_stream_tee_branch_cancel_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let reader_obj = match slots
    .get(READABLE_STREAM_TEE_BRANCH_CANCEL_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee branch cancel missing reader slot",
      ))
    }
  };
  let other_branch_obj = match slots
    .get(READABLE_STREAM_TEE_BRANCH_CANCEL_SLOT_OTHER_BRANCH)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee branch cancel missing other-branch slot",
      ))
    }
  };

  // First cancel the branch itself (this throws if locked, matching `ReadableStream.cancel`).
  let promise = readable_stream_cancel_native(vm, scope, host, hooks, callee, this, &[])?;

  // If both branches are canceled, cancel the original reader to stop pumping.
  let other_closed = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(
      state
        .streams
        .get(&WeakGcObject::from(other_branch_obj))
        .map_or(true, |s| s.state != StreamLifecycleState::Readable),
    )
  })?;
  if other_closed {
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
  }

  Ok(promise)
}

fn readable_stream_tee_read_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let branch0_obj = match slots
    .get(READABLE_STREAM_TEE_SLOT_BRANCH0)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee read rejected callback missing branch0 slot",
      ))
    }
  };
  let branch1_obj = match slots
    .get(READABLE_STREAM_TEE_SLOT_BRANCH1)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee read rejected callback missing branch1 slot",
      ))
    }
  };

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let msg = best_effort_reason_string(scope, reason, "ReadableStream tee rejected")?;

  let pending0 = error_readable_stream(vm, scope, callee, branch0_obj, msg.clone())?;
  if let Some(pending) = pending0 {
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

  let pending1 = error_readable_stream(vm, scope, callee, branch1_obj, msg)?;
  if let Some(pending) = pending1 {
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

fn readable_stream_tee_read_fulfilled_native(
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
    .get(READABLE_STREAM_TEE_SLOT_READER)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee read fulfilled callback missing reader slot",
      ))
    }
  };
  let branch0_obj = match slots
    .get(READABLE_STREAM_TEE_SLOT_BRANCH0)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee read fulfilled callback missing branch0 slot",
      ))
    }
  };
  let branch1_obj = match slots
    .get(READABLE_STREAM_TEE_SLOT_BRANCH1)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "ReadableStream tee read fulfilled callback missing branch1 slot",
      ))
    }
  };
  let realm_slot = slots
    .get(STREAM_REALM_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);

  // If both branches are already canceled, cancel the original reader and stop pumping.
  let (branch0_open, branch1_open) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let b0 = state
      .streams
      .get(&WeakGcObject::from(branch0_obj))
      .map_or(false, |s| s.state == StreamLifecycleState::Readable);
    let b1 = state
      .streams
      .get(&WeakGcObject::from(branch1_obj))
      .map_or(false, |s| s.state == StreamLifecycleState::Readable);
    Ok((b0, b1))
  })?;

  if !branch0_open && !branch1_open {
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
    return Ok(Value::Undefined);
  }

  let result = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(result_obj) = result else {
    let msg = "ReadableStream.tee: read() did not return an object".to_string();
    let pending0 = error_readable_stream(vm, scope, callee, branch0_obj, msg.clone())?;
    if let Some(pending) = pending0 {
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
    let pending1 = error_readable_stream(vm, scope, callee, branch1_obj, msg)?;
    if let Some(pending) = pending1 {
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
  };

  // Root result object while allocating keys / accessing properties.
  scope.push_root(Value::Object(result_obj))?;
  let done_key = alloc_key(scope, "done")?;
  let done_val = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, done_key)?;
  let done = scope.heap().to_boolean(done_val)?;

  if done {
    let pending0 = close_readable_stream(vm, scope, callee, branch0_obj)?;
    if let Some(pending) = pending0 {
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

    let pending1 = close_readable_stream(vm, scope, callee, branch1_obj)?;
    if let Some(pending) = pending1 {
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

  let value_key = alloc_key(scope, "value")?;
  let chunk_val = vm.get_with_host_and_hooks(host, scope, hooks, result_obj, value_key)?;
  enum TeeChunk {
    Bytes(Vec<u8>),
    String(String),
    Value(Value),
  }
  let chunk = match chunk_val {
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => {
      TeeChunk::Bytes(scope.heap().uint8_array_data(obj)?.to_vec())
    }
    Value::String(s) => {
      let code_units = scope.heap().get_string(s)?.as_code_units();
      let byte_len = utf8_len_from_utf16_units(code_units)?;
      if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
        let msg = "ReadableStream.tee: chunk too large".to_string();
        let pending0 = error_readable_stream(vm, scope, callee, branch0_obj, msg.clone())?;
        if let Some(pending) = pending0 {
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
        let pending1 = error_readable_stream(vm, scope, callee, branch1_obj, msg)?;
        if let Some(pending) = pending1 {
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
      TeeChunk::String(utf16_units_to_utf8_string_lossy(code_units, byte_len)?)
    }
    _ => TeeChunk::Value(chunk_val),
  };

  // Enqueue into each non-canceled branch.
  match chunk {
    TeeChunk::Bytes(bytes) => {
      if branch0_open {
        let pending =
          enqueue_bytes_into_readable_stream(vm, scope, callee, branch0_obj, bytes.clone())?;
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
      }
      if branch1_open {
        let pending = enqueue_bytes_into_readable_stream(vm, scope, callee, branch1_obj, bytes)?;
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
      }
    }
    TeeChunk::String(s) => {
      if branch0_open {
        let pending =
          enqueue_string_into_readable_stream(vm, scope, callee, branch0_obj, s.clone())?;
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
      }
      if branch1_open {
        let pending = enqueue_string_into_readable_stream(vm, scope, callee, branch1_obj, s)?;
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
      }
    }
    TeeChunk::Value(value) => {
      if branch0_open {
        let pending =
          enqueue_value_into_readable_stream(vm, scope, callee, branch0_obj, value)?;
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
      }
      if branch1_open {
        let pending = enqueue_value_into_readable_stream(vm, scope, callee, branch1_obj, value)?;
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
      }
    }
  }

  // Continue pumping: reader.read().then(on_fulfilled, on_rejected)
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
      let msg = err.to_string();
      let pending0 = error_readable_stream(vm, scope, callee, branch0_obj, msg.clone())?;
      if let Some(pending) = pending0 {
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
      let pending1 = error_readable_stream(vm, scope, callee, branch1_obj, msg)?;
      if let Some(pending) = pending1 {
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
  };

  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return a Promise",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_tee_read_fulfilled_call_id,
        state.readable_stream_tee_read_rejected_call_id,
      ))
    })?;

  // Root captured values + promise across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(branch0_obj))?;
  scope.push_root(Value::Object(branch1_obj))?;
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream tee read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch0_obj),
      Value::Object(branch1_obj),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream tee read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch0_obj),
      Value::Object(branch1_obj),
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

fn readable_stream_tee_native(
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
    "ReadableStream.tee: illegal invocation",
  )?;

  // Root the source stream across allocations in this helper. The VM call frame roots `this`, but we
  // also invoke other native helpers directly from Rust, which can allocate/GC before they root
  // their receivers.
  scope.push_root(Value::Object(stream_obj))?;

  // Ensure this is one of our streams (and not e.g. an arbitrary object inheriting the prototype),
  // and that it isn't already locked.
  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream.tee: illegal invocation"))?;
    if stream_state.locked {
      return Err(VmError::TypeError("ReadableStream is locked"));
    }
    Ok(())
  })?;

  // Lock the source stream by acquiring a reader.
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
  scope.push_root(Value::Object(reader_obj))?;

  // Create two dynamic byte streams to act as the tee branches.
  let branch0 = create_readable_byte_stream_dynamic(vm, scope, callee)?;
  scope.push_root(Value::Object(branch0))?;
  let branch1 = create_readable_byte_stream_dynamic(vm, scope, callee)?;
  scope.push_root(Value::Object(branch1))?;

  // Override `cancel()` on each branch so we can propagate cancellation back to the original reader
  // when both branches cancel.
  let cancel_call_id = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    Ok(state.readable_stream_tee_branch_cancel_call_id)
  })?;
  let intr = require_intrinsics(vm, "ReadableStream requires intrinsics")?;
  let func_proto = intr.function_prototype();
  let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
  let realm_slot = Value::Number(realm_id.to_raw() as f64);

  // Root captured values across callback allocation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader_obj))?;
  scope.push_root(Value::Object(branch0))?;
  scope.push_root(Value::Object(branch1))?;

  let cancel0_name = scope.alloc_string("cancel")?;
  scope.push_root(Value::String(cancel0_name))?;
  let cancel0 = scope.alloc_native_function_with_slots(
    cancel_call_id,
    None,
    cancel0_name,
    0,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch1),
    ],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(cancel0, Some(func_proto))?;
  set_data_prop(&mut scope, branch0, "cancel", Value::Object(cancel0), true)?;

  let cancel1_name = scope.alloc_string("cancel")?;
  scope.push_root(Value::String(cancel1_name))?;
  let cancel1 = scope.alloc_native_function_with_slots(
    cancel_call_id,
    None,
    cancel1_name,
    0,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch0),
    ],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(cancel1, Some(func_proto))?;
  set_data_prop(&mut scope, branch1, "cancel", Value::Object(cancel1), true)?;

  // Start the tee pump: `reader.read().then(enqueue-into-branches, error-branches)`.
  let read_promise = reader_read_native(
    vm,
    &mut scope,
    host,
    hooks,
    callee,
    Value::Object(reader_obj),
    &[],
  )?;
  let Value::Object(read_promise_obj) = read_promise else {
    return Err(VmError::InvariantViolation(
      "ReadableStreamDefaultReader.read must return a Promise",
    ));
  };

  let (fulfilled_call_id, rejected_call_id) =
    with_realm_state_mut(vm, &mut scope, callee, |state, _heap| {
      Ok((
        state.readable_stream_tee_read_fulfilled_call_id,
        state.readable_stream_tee_read_rejected_call_id,
      ))
    })?;

  // Root promise across callback allocation.
  scope.push_root(read_promise)?;

  let on_fulfilled_name = scope.alloc_string("ReadableStream tee read fulfilled")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch0),
      Value::Object(branch1),
    ],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("ReadableStream tee read rejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[
      realm_slot,
      Value::Object(reader_obj),
      Value::Object(branch0),
      Value::Object(branch1),
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

  // Return an array-like object with indices `0` and `1`.
  let branches = scope.alloc_array(2)?;
  scope.push_root(Value::Object(branches))?;
  scope
    .heap_mut()
    .object_set_prototype(branches, Some(intr.array_prototype()))?;
  let branch0_key = alloc_key(&mut scope, "0")?;
  let branch1_key = alloc_key(&mut scope, "1")?;
  scope.define_property(
    branches,
    branch0_key,
    result_data_desc(Value::Object(branch0)),
  )?;
  scope.define_property(
    branches,
    branch1_key,
    result_data_desc(Value::Object(branch1)),
  )?;

  Ok(Value::Object(branches))
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

  // Resolve kind based on the stream's current kind and (for uninitialized streams) the first
  // enqueued chunk type.
  let (
    kind,
    lifecycle_state,
    close_requested,
    buffered_byte_len,
    byte_queue_len,
    buffered_string_len,
    string_queue_len,
  ) =
    with_realm_state_mut(vm, scope, callee, |state, _heap| {
      let stream_state = state
        .streams
        .get(&WeakGcObject::from(stream_obj))
        .ok_or(VmError::TypeError(
          "ReadableStreamDefaultController.enqueue: invalid stream",
        ))?;

      Ok((
        stream_state.kind,
        stream_state.state,
        stream_state.close_requested,
        stream_state.buffered_byte_len,
        stream_state.byte_queue.len(),
        stream_state.buffered_string_len,
        stream_state.strings.len(),
      ))
    })?;

  if lifecycle_state != StreamLifecycleState::Readable || close_requested {
    return Err(VmError::TypeError("ReadableStream is closed"));
  }

  let pending = match kind {
    StreamKind::Values => enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?,
    StreamKind::Bytes => {
      if let Some(byte_len) = buffer_source_byte_length(scope, chunk)? {
        let new_total = buffered_byte_len.checked_add(byte_len).ok_or(VmError::OutOfMemory)?;
        let additional_chunks = if byte_len == 0 || byte_len <= STREAM_CHUNK_BYTES {
          1
        } else {
          byte_len
            .checked_add(STREAM_CHUNK_BYTES - 1)
            .ok_or(VmError::OutOfMemory)?
            / STREAM_CHUNK_BYTES
        };
        let new_chunk_total = byte_queue_len
          .checked_add(additional_chunks)
          .ok_or(VmError::OutOfMemory)?;

        if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
          || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
        {
          let pending = error_readable_stream(
            vm,
            scope,
            callee,
            stream_obj,
            READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string(),
          )?;
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

        let Some(bytes) = extract_buffer_source_bytes(scope, chunk)? else {
          return Err(VmError::InvariantViolation(
            "ReadableStream BufferSource byte length detected but extraction failed",
          ));
        };
        enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?
      } else {
        // For non-BufferSource chunks, fall back to object-mode streams (values).
        enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?
      }
    }
    StreamKind::Strings => match chunk {
      Value::String(s) => {
        let code_units = scope.heap().get_string(s)?.as_code_units();
        let byte_len = utf8_len_from_utf16_units(code_units)?;
        if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
          return Err(VmError::TypeError("ReadableStream chunk too large"));
        }

        let new_total = buffered_string_len
          .checked_add(byte_len)
          .ok_or(VmError::OutOfMemory)?;
        let new_chunk_total = string_queue_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
        if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
          || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
        {
          let pending = error_readable_stream(
            vm,
            scope,
            callee,
            stream_obj,
            READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string(),
          )?;
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

        let chunk_string = utf16_units_to_utf8_string_lossy(code_units, byte_len)?;
        enqueue_string_into_readable_stream(vm, scope, callee, stream_obj, chunk_string)?
      }
      _ => enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?,
    },
    StreamKind::Uninitialized => {
      if let Some(byte_len) = buffer_source_byte_length(scope, chunk)? {
        let new_total = buffered_byte_len.checked_add(byte_len).ok_or(VmError::OutOfMemory)?;
        let additional_chunks = if byte_len == 0 || byte_len <= STREAM_CHUNK_BYTES {
          1
        } else {
          byte_len
            .checked_add(STREAM_CHUNK_BYTES - 1)
            .ok_or(VmError::OutOfMemory)?
            / STREAM_CHUNK_BYTES
        };
        let new_chunk_total = byte_queue_len
          .checked_add(additional_chunks)
          .ok_or(VmError::OutOfMemory)?;

        if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
          || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
        {
          let pending = error_readable_stream(
            vm,
            scope,
            callee,
            stream_obj,
            READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string(),
          )?;
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

        let Some(bytes) = extract_buffer_source_bytes(scope, chunk)? else {
          return Err(VmError::InvariantViolation(
            "ReadableStream BufferSource byte length detected but extraction failed",
          ));
        };
        enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?
      } else if let Value::String(s) = chunk {
        let code_units = scope.heap().get_string(s)?.as_code_units();
        let byte_len = utf8_len_from_utf16_units(code_units)?;
        if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
          return Err(VmError::TypeError("ReadableStream chunk too large"));
        }

        let new_total = buffered_string_len
          .checked_add(byte_len)
          .ok_or(VmError::OutOfMemory)?;
        let new_chunk_total = string_queue_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
        if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
          || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
        {
          let pending = error_readable_stream(
            vm,
            scope,
            callee,
            stream_obj,
            READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string(),
          )?;
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

        let chunk_string = utf16_units_to_utf8_string_lossy(code_units, byte_len)?;
        enqueue_string_into_readable_stream(vm, scope, callee, stream_obj, chunk_string)?
      } else {
        enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?
      }
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
  // Preserve the JS error reason for spec-compliant consumption by `cancel()` / `read()` paths.
  // (This is also required so cancel rejects with the same value, not a stringified message.)
  set_data_prop(
    scope,
    stream_obj,
    READABLE_STREAM_STORED_ERROR_KEY,
    reason,
    true,
  )?;
  let msg = best_effort_reason_string(scope, reason, "ReadableStream errored")?;

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

fn readable_stream_controller_desired_size_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let controller_obj = require_readable_stream_controller(scope, this)?;
  let stream_obj = readable_stream_controller_stream(scope, controller_obj)?;

  with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultController: illegal invocation",
      ))?;

    // Match the web-platform shape: closed/errored streams report `null`.
    if stream_state.state != StreamLifecycleState::Readable {
      return Ok(Value::Null);
    }

    let queue_size: f64 = match stream_state.kind {
      StreamKind::Bytes => stream_state.buffered_byte_len as f64,
      StreamKind::Strings => stream_state.strings.len() as f64,
      StreamKind::Values => stream_state.values.len() as f64,
      StreamKind::Uninitialized => 0.0,
    };

    Ok(Value::Number(stream_state.high_water_mark - queue_size))
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
  Value(RootId),
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
      ReadChunk::Value(root_id) => {
        let value = scope
          .heap()
          .get_root(root_id)
          .ok_or(VmError::InvariantViolation(
            "ReadableStream value chunk root missing",
          ))?;
        scope.push_root(value)?;

        let settle_result = (|| {
          let result = scope.alloc_object()?;
          scope.push_root(Value::Object(result))?;

          let value_key = alloc_key(scope, "value")?;
          let done_key = alloc_key(scope, "done")?;

          scope.define_property(result, value_key, result_data_desc(value))?;
          scope.define_property(result, done_key, result_data_desc(Value::Bool(false)))?;

          vm.call_with_host_and_hooks(
            host,
            scope,
            hooks,
            resolve,
            Value::Undefined,
            &[Value::Object(result)],
          )?;

          Ok(())
        })();

        // Always remove the persistent root, even if settlement failed (avoid leaking roots on
        // errors).
        scope.heap_mut().remove_root(root_id);

        settle_result?;
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
              if bytes.len() > MAX_READABLE_STREAM_QUEUED_BYTES {
                stream_state.state = StreamLifecycleState::Errored;
                let msg = READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string();
                stream_state.error_message = Some(msg.clone());
                return Ok((ReadOutcome::Error(msg), None));
              }
              let chunk_count = if bytes.is_empty() {
                0
              } else if bytes.len() <= STREAM_CHUNK_BYTES {
                1
              } else {
                bytes
                  .len()
                  .checked_add(STREAM_CHUNK_BYTES - 1)
                  .ok_or(VmError::OutOfMemory)?
                  / STREAM_CHUNK_BYTES
              };
              if chunk_count > MAX_READABLE_STREAM_QUEUED_CHUNKS {
                stream_state.state = StreamLifecycleState::Errored;
                let msg = READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string();
                stream_state.error_message = Some(msg.clone());
                return Ok((ReadOutcome::Error(msg), None));
              }

              let (byte_queue, buffered_byte_len) = chunk_bytes(bytes)?;
              stream_state.byte_queue = byte_queue;
              stream_state.buffered_byte_len = buffered_byte_len;
            }
            Err(err) => {
              stream_state.state = StreamLifecycleState::Errored;
              let msg = err.to_string();
              stream_state.error_message = Some(msg.clone());
              return Ok((ReadOutcome::Error(msg), None));
            }
          }
        }

        if stream_state.byte_queue.is_empty() {
          if stream_state.close_requested {
            stream_state.state = StreamLifecycleState::Closed;
            return Ok((ReadOutcome::Done, None));
          }

          // No bytes available yet: keep the promise pending and resolve it when bytes are enqueued.
          stream_state.pending_reader = Some(WeakGcObject::from(reader_obj));
          return Ok((ReadOutcome::Pending, Some(stream_weak)));
        }

        let Some(chunk) = stream_state.byte_queue.pop_front() else {
          return Ok((
            ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
            None,
          ));
        };
        if chunk.len() > stream_state.buffered_byte_len {
          stream_state.buffered_byte_len = 0;
          return Ok((
            ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
            None,
          ));
        }
        stream_state.buffered_byte_len -= chunk.len();

        if stream_state.close_requested && stream_state.byte_queue.is_empty() {
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
        stream_state.buffered_string_len = stream_state.buffered_string_len.saturating_sub(chunk.len());

        if stream_state.close_requested && stream_state.strings.is_empty() {
          stream_state.state = StreamLifecycleState::Closed;
        }

        Ok((ReadOutcome::Chunk(ReadChunk::String(chunk)), None))
      }
      StreamKind::Values => {
        if stream_state.values.is_empty() {
          if stream_state.close_requested {
            stream_state.state = StreamLifecycleState::Closed;
            return Ok((ReadOutcome::Done, None));
          }

          stream_state.pending_reader = Some(WeakGcObject::from(reader_obj));
          return Ok((ReadOutcome::Pending, Some(stream_weak)));
        }

        let Some(root_id) = stream_state.values.pop_front() else {
          return Ok((
            ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
            None,
          ));
        };

        if stream_state.close_requested && stream_state.values.is_empty() {
          stream_state.state = StreamLifecycleState::Closed;
        }

        Ok((ReadOutcome::Chunk(ReadChunk::Value(root_id)), None))
      }
      StreamKind::Uninitialized => {
        // Streams created by the JS `ReadableStream` constructor with an underlying source `start`
        // callback can be either string streams or byte streams, depending on the first enqueued
        // chunk.
        //
        // If a read happens before the first `enqueue`, keep it pending until we know the stream
        // kind and have data (or the stream closes/errors).
        if stream_state.close_requested {
          stream_state.state = StreamLifecycleState::Closed;
          return Ok((ReadOutcome::Done, None));
        }

        stream_state.pending_reader = Some(WeakGcObject::from(reader_obj));
        Ok((ReadOutcome::Pending, Some(stream_weak)))
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
  enum CancelAction {
    Resolve { value_roots: Vec<RootId> },
    Reject {
      stream: WeakGcObject,
      error_message: Option<String>,
    },
  }

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

  let (action, pending_read) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let reader_state = state
      .readers
      .get(&WeakGcObject::from(reader_obj))
      .ok_or(VmError::TypeError(
        "ReadableStreamDefaultReader.cancel: illegal invocation",
      ))?;
    let Some(stream_weak) = reader_state.stream else {
      return Ok((CancelAction::Resolve { value_roots: Vec::new() }, None));
    };
    let Some(stream_state) = state.streams.get_mut(&stream_weak) else {
      return Ok((CancelAction::Resolve { value_roots: Vec::new() }, None));
    };

    match stream_state.state {
      StreamLifecycleState::Closed => {
        return Ok((CancelAction::Resolve { value_roots: Vec::new() }, None))
      }
      StreamLifecycleState::Errored => {
        return Ok((
          CancelAction::Reject {
            stream: stream_weak,
            error_message: stream_state.error_message.clone(),
          },
          None,
        ))
      }
      StreamLifecycleState::Readable => {}
    };

    if let Some(init) = stream_state.init.take() {
      let _ = init();
    }

    stream_state.state = StreamLifecycleState::Closed;
    stream_state.close_requested = true;
    stream_state.error_message = None;
    stream_state.byte_queue.clear();
    stream_state.buffered_byte_len = 0;
    stream_state.strings.clear();
    stream_state.buffered_string_len = 0;
    let value_roots: Vec<RootId> = stream_state.values.drain(..).collect();
    stream_state.init = None;
    let pending_reader = stream_state.pending_reader.take();

    let pending_read = pending_reader.map(|reader| PendingReadSettle {
      reader,
      roots: stream_state.pending_read_roots.take(),
      outcome: ReadOutcome::Done,
    });
    if pending_read.is_none() {
      debug_assert!(stream_state.pending_read_roots.is_none());
    }

    Ok((CancelAction::Resolve { value_roots }, pending_read))
  })?;

  match action {
    CancelAction::Resolve { value_roots } => {
      for root_id in value_roots {
        scope.heap_mut().remove_root(root_id);
      }
      vm.call_with_host_and_hooks(host, scope, hooks, resolve, Value::Undefined, &[])?;
    }
    CancelAction::Reject {
      stream,
      error_message,
    } => {
      let Some(stream_obj) = stream.upgrade(scope.heap()) else {
        return Err(VmError::InvariantViolation(
          "ReadableStreamDefaultReader.cancel: stream has been garbage collected",
        ));
      };
      // Prefer the stored JS error reason (set by controller.error or internal error paths).
      let reason = match get_data_prop_opt(scope, stream_obj, READABLE_STREAM_STORED_ERROR_KEY)? {
        Some(v) => v,
        None => match error_message {
          Some(msg) => Value::String(scope.alloc_string(&msg)?),
          None => Value::Undefined,
        },
      };
      scope.push_root(reason)?;
      vm.call_with_host_and_hooks(host, scope, hooks, reject, Value::Undefined, &[reason])?;
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
      // Dynamic streams may become either byte streams or string streams depending on the first
      // enqueued chunk (e.g. `TransformStream` may output strings via `TextDecoderStream`).
      .insert(
        WeakGcObject::from(obj),
        StreamState::new_empty_uninitialized(DEFAULT_READABLE_STREAM_HIGH_WATER_MARK),
      );
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
  let (pending, value_roots) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

    match stream_state.kind {
      StreamKind::Bytes => {}
      StreamKind::Uninitialized => {
        stream_state.kind = StreamKind::Bytes;
      }
      StreamKind::Strings | StreamKind::Values => {
        return Err(VmError::TypeError(
          "ReadableStream enqueue expects byte streams",
        ));
      }
    }

    if stream_state.kind != StreamKind::Bytes {
      return Err(VmError::TypeError(
        "ReadableStream enqueue expects byte streams",
      ));
    }

    if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
      return Err(VmError::TypeError("ReadableStream is closed"));
    }

    let byte_len = bytes.len();
    let new_total = stream_state
      .buffered_byte_len
      .checked_add(byte_len)
      .ok_or(VmError::OutOfMemory)?;
    let additional_chunks = if byte_len == 0 || byte_len <= STREAM_CHUNK_BYTES {
      1
    } else {
      byte_len
        .checked_add(STREAM_CHUNK_BYTES - 1)
        .ok_or(VmError::OutOfMemory)?
        / STREAM_CHUNK_BYTES
    };
    let new_chunk_total = stream_state
      .byte_queue
      .len()
      .checked_add(additional_chunks)
      .ok_or(VmError::OutOfMemory)?;

    if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
      || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
    {
      let error_message = READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string();
      stream_state.state = StreamLifecycleState::Errored;
      stream_state.close_requested = true;
      stream_state.error_message = Some(error_message.clone());
      stream_state.byte_queue.clear();
      stream_state.buffered_byte_len = 0;
      stream_state.strings.clear();
      stream_state.buffered_string_len = 0;
      let value_roots: Vec<RootId> = stream_state.values.drain(..).collect();
      stream_state.init = None;

      let pending_reader = stream_state.pending_reader.take();
      if let Some(reader) = pending_reader {
        let roots = stream_state.pending_read_roots.take();
        return Ok((
          Some(PendingReadSettle {
            reader,
            roots,
            outcome: ReadOutcome::Error(error_message),
          }),
          value_roots,
        ));
      }

      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok((None, value_roots));
    }

    push_byte_chunks(
      &mut stream_state.byte_queue,
      &mut stream_state.buffered_byte_len,
      bytes,
    )?;

    let Some(pending_reader) = stream_state.pending_reader.take() else {
      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok((None, Vec::new()));
    };
    let pending_roots = stream_state.pending_read_roots.take();

    let Some(chunk) = stream_state.byte_queue.pop_front() else {
      return Ok((
        Some(PendingReadSettle {
          reader: pending_reader,
          roots: pending_roots,
          outcome: ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
        }),
        Vec::new(),
      ));
    };
    if chunk.len() > stream_state.buffered_byte_len {
      stream_state.buffered_byte_len = 0;
      return Ok((
        Some(PendingReadSettle {
          reader: pending_reader,
          roots: pending_roots,
          outcome: ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
        }),
        Vec::new(),
      ));
    }
    stream_state.buffered_byte_len -= chunk.len();

    Ok((
      Some(PendingReadSettle {
        reader: pending_reader,
        roots: pending_roots,
        outcome: ReadOutcome::Chunk(ReadChunk::Bytes(chunk)),
      }),
      Vec::new(),
    ))
  })?;

  for root_id in value_roots {
    scope.heap_mut().remove_root(root_id);
  }

  Ok(pending)
}

fn enqueue_string_into_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
  chunk: String,
) -> Result<Option<PendingReadSettle>, VmError> {
  let (pending, value_roots) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

    match stream_state.kind {
      StreamKind::Strings => {}
      StreamKind::Uninitialized => {
        stream_state.kind = StreamKind::Strings;
      }
      StreamKind::Bytes | StreamKind::Values => {
        return Err(VmError::TypeError(
          "ReadableStream enqueue expects string streams",
        ));
      }
    }

    if stream_state.kind != StreamKind::Strings {
      return Err(VmError::TypeError(
        "ReadableStream enqueue expects string streams",
      ));
    }

    if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
      return Err(VmError::TypeError("ReadableStream is closed"));
    }

    let chunk_len = chunk.len();
    let new_total = stream_state
      .buffered_string_len
      .checked_add(chunk_len)
      .ok_or(VmError::OutOfMemory)?;
    let new_chunk_total = stream_state
      .strings
      .len()
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    if new_total > MAX_READABLE_STREAM_QUEUED_BYTES
      || new_chunk_total > MAX_READABLE_STREAM_QUEUED_CHUNKS
    {
      let error_message = READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string();
      stream_state.state = StreamLifecycleState::Errored;
      stream_state.close_requested = true;
      stream_state.error_message = Some(error_message.clone());
      stream_state.byte_queue.clear();
      stream_state.buffered_byte_len = 0;
      stream_state.strings.clear();
      stream_state.buffered_string_len = 0;
      let value_roots: Vec<RootId> = stream_state.values.drain(..).collect();
      stream_state.init = None;

      let pending_reader = stream_state.pending_reader.take();
      if let Some(reader) = pending_reader {
        let roots = stream_state.pending_read_roots.take();
        return Ok((
          Some(PendingReadSettle {
            reader,
            roots,
            outcome: ReadOutcome::Error(error_message),
          }),
          value_roots,
        ));
      }

      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok((None, value_roots));
    }

    stream_state.buffered_string_len = new_total;
    stream_state.strings.push_back(chunk);

    let Some(pending_reader) = stream_state.pending_reader.take() else {
      debug_assert!(stream_state.pending_read_roots.is_none());
      return Ok((None, Vec::new()));
    };
    let pending_roots = stream_state.pending_read_roots.take();

    let Some(chunk) = stream_state.strings.pop_front() else {
      return Ok((
        Some(PendingReadSettle {
          reader: pending_reader,
          roots: pending_roots,
          outcome: ReadOutcome::Error("ReadableStream internal queue invariant violated".to_string()),
        }),
        Vec::new(),
      ));
    };
    stream_state.buffered_string_len = stream_state.buffered_string_len.saturating_sub(chunk.len());

    Ok((
      Some(PendingReadSettle {
        reader: pending_reader,
        roots: pending_roots,
        outcome: ReadOutcome::Chunk(ReadChunk::String(chunk)),
      }),
      Vec::new(),
    ))
  })?;

  for root_id in value_roots {
    scope.heap_mut().remove_root(root_id);
  }

  Ok(pending)
}

fn enqueue_value_into_readable_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  stream_obj: GcObject,
  chunk: Value,
) -> Result<Option<PendingReadSettle>, VmError> {
  let would_exceed_item_cap =
    with_realm_state_mut(vm, scope, callee, |state, _heap| -> Result<_, VmError> {
      let stream_state = state
        .streams
        .get(&WeakGcObject::from(stream_obj))
        .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

      if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
        return Err(VmError::TypeError("ReadableStream is closed"));
      }

      let queued_items_after_enqueue = match stream_state.kind {
        StreamKind::Values => stream_state.values.len().saturating_add(1),
        StreamKind::Uninitialized => 1,
        StreamKind::Strings => stream_state.strings.len().saturating_add(1),
        StreamKind::Bytes => stream_state.byte_queue.len().saturating_add(1),
      };

      Ok(queued_items_after_enqueue > MAX_READABLE_STREAM_QUEUED_ITEMS)
    })?;

  if would_exceed_item_cap {
    return error_readable_stream(
      vm,
      scope,
      callee,
      stream_obj,
      READABLE_STREAM_QUEUE_LIMIT_ERROR.to_string(),
    );
  }

  struct PromoteToValues {
    from_kind: StreamKind,
    strings: VecDeque<String>,
    buffered_string_len: usize,
    byte_queue: VecDeque<Vec<u8>>,
    buffered_byte_len: usize,
    pending_reader: Option<WeakGcObject>,
    pending_roots: Option<PendingReadRoots>,
  }

  enum EnqueueAction {
    Enqueued(Option<PendingReadSettle>),
    Promote(PromoteToValues),
  }

  // Root the value until it is read (or the stream is canceled/errored/GC'd).
  let chunk_root = scope.heap_mut().add_root(chunk)?;

  let action: Result<EnqueueAction, VmError> = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

    if stream_state.state != StreamLifecycleState::Readable || stream_state.close_requested {
      return Err(VmError::TypeError("ReadableStream is closed"));
    }

    match stream_state.kind {
      StreamKind::Values => {
        stream_state.values.push_back(chunk_root);

        let Some(pending_reader) = stream_state.pending_reader.take() else {
          debug_assert!(stream_state.pending_read_roots.is_none());
          return Ok(EnqueueAction::Enqueued(None));
        };
        let pending_roots = stream_state.pending_read_roots.take();

        let Some(root_id) = stream_state.values.pop_front() else {
          return Ok(EnqueueAction::Enqueued(Some(PendingReadSettle {
            reader: pending_reader,
            roots: pending_roots,
            outcome: ReadOutcome::Error(
              "ReadableStream internal queue invariant violated".to_string(),
            ),
          })));
        };

        Ok(EnqueueAction::Enqueued(Some(PendingReadSettle {
          reader: pending_reader,
          roots: pending_roots,
          outcome: ReadOutcome::Chunk(ReadChunk::Value(root_id)),
        })))
      }
      StreamKind::Uninitialized => {
        stream_state.kind = StreamKind::Values;
        stream_state.values.push_back(chunk_root);

        let Some(pending_reader) = stream_state.pending_reader.take() else {
          debug_assert!(stream_state.pending_read_roots.is_none());
          return Ok(EnqueueAction::Enqueued(None));
        };
        let pending_roots = stream_state.pending_read_roots.take();

        let Some(root_id) = stream_state.values.pop_front() else {
          return Ok(EnqueueAction::Enqueued(Some(PendingReadSettle {
            reader: pending_reader,
            roots: pending_roots,
            outcome: ReadOutcome::Error(
              "ReadableStream internal queue invariant violated".to_string(),
            ),
          })));
        };

        Ok(EnqueueAction::Enqueued(Some(PendingReadSettle {
          reader: pending_reader,
          roots: pending_roots,
          outcome: ReadOutcome::Chunk(ReadChunk::Value(root_id)),
        })))
      }
      StreamKind::Strings => {
        let strings: VecDeque<String> = stream_state.strings.drain(..).collect();
        let buffered_string_len = stream_state.buffered_string_len;
        stream_state.buffered_string_len = 0;
        let pending_reader = stream_state.pending_reader.take();
        let pending_roots = stream_state.pending_read_roots.take();
        stream_state.kind = StreamKind::Values;
        Ok(EnqueueAction::Promote(PromoteToValues {
          from_kind: StreamKind::Strings,
          strings,
          buffered_string_len,
          byte_queue: VecDeque::new(),
          buffered_byte_len: 0,
          pending_reader,
          pending_roots,
        }))
      }
      StreamKind::Bytes => {
        let byte_queue: VecDeque<Vec<u8>> = stream_state.byte_queue.drain(..).collect();
        let buffered_byte_len = stream_state.buffered_byte_len;
        stream_state.buffered_byte_len = 0;
        let pending_reader = stream_state.pending_reader.take();
        let pending_roots = stream_state.pending_read_roots.take();
        stream_state.kind = StreamKind::Values;
        Ok(EnqueueAction::Promote(PromoteToValues {
          from_kind: StreamKind::Bytes,
          strings: VecDeque::new(),
          buffered_string_len: 0,
          byte_queue,
          buffered_byte_len,
          pending_reader,
          pending_roots,
        }))
      }
    }
  });

  let action = match action {
    Ok(action) => action,
    Err(err) => {
      scope.heap_mut().remove_root(chunk_root);
      return Err(err);
    }
  };

  match action {
    EnqueueAction::Enqueued(pending) => Ok(pending),
    EnqueueAction::Promote(promotion) => {
      let mut promoted_existing_roots: Vec<RootId> = Vec::new();

      let promote_and_enqueue: Result<Option<PendingReadSettle>, VmError> = (|| {
        // Promote any existing queued chunks into rooted JS values.
        promoted_existing_roots
          .try_reserve_exact(promotion.strings.len().saturating_add(promotion.byte_queue.len()))
          .map_err(|_| VmError::OutOfMemory)?;

        // Convert queued string chunks back into JS strings.
        for s in promotion.strings.iter() {
          let js_s = scope.alloc_string(s)?;
          scope.push_root(Value::String(js_s))?;
          let root_id = scope.heap_mut().add_root(Value::String(js_s))?;
          promoted_existing_roots.push(root_id);
        }

        // Convert queued byte chunks into `Uint8Array` JS values.
        if !promotion.byte_queue.is_empty() {
          let intr = require_intrinsics(vm, "ReadableStream requires intrinsics")?;
          for bytes in promotion.byte_queue.iter() {
            // Preserve the original bytes queue for restoration by cloning.
            let bytes = bytes.clone();
            let byte_len = bytes.len();

            let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
            scope.push_root(Value::Object(ab))?;
            scope
              .heap_mut()
              .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

            let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
            scope.push_root(Value::Object(view))?;
            scope
              .heap_mut()
              .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;

            let root_id = scope.heap_mut().add_root(Value::Object(view))?;
            promoted_existing_roots.push(root_id);
          }
        }

        with_realm_state_mut(vm, scope, callee, |state, _heap| -> Result<_, VmError> {
          let stream_state = state
            .streams
            .get_mut(&WeakGcObject::from(stream_obj))
            .ok_or(VmError::TypeError("ReadableStream enqueue: invalid stream"))?;

          if stream_state.kind != StreamKind::Values {
            return Err(VmError::InvariantViolation(
              "ReadableStream stream kind changed while promoting to value stream",
            ));
          }

          stream_state
            .values
            .try_reserve(promoted_existing_roots.len().saturating_add(1))
            .map_err(|_| VmError::OutOfMemory)?;
          for root_id in promoted_existing_roots.iter().copied() {
            stream_state.values.push_back(root_id);
          }
          stream_state.values.push_back(chunk_root);

          let Some(pending_reader) = promotion.pending_reader else {
            debug_assert!(promotion.pending_roots.is_none());
            return Ok(None);
          };

          // Settle pending read with the oldest enqueued value (now in the values queue).
          let pending_roots = promotion.pending_roots;
          let Some(root_id) = stream_state.values.pop_front() else {
            return Ok(Some(PendingReadSettle {
              reader: pending_reader,
              roots: pending_roots,
              outcome: ReadOutcome::Error(
                "ReadableStream internal queue invariant violated".to_string(),
              ),
            }));
          };

          Ok(Some(PendingReadSettle {
            reader: pending_reader,
            roots: pending_roots,
            outcome: ReadOutcome::Chunk(ReadChunk::Value(root_id)),
          }))
        })
      })();

      match promote_and_enqueue {
        Ok(pending) => Ok(pending),
        Err(err) => {
          // Clean up any promoted persistent roots created so far.
          for root_id in promoted_existing_roots.drain(..) {
            scope.heap_mut().remove_root(root_id);
          }

          // Restore the original stream kind + queues.
          let _ = with_realm_state_mut(vm, scope, callee, |state, _heap| {
            let stream_state = match state.streams.get_mut(&WeakGcObject::from(stream_obj)) {
              Some(s) => s,
              None => return Ok(()),
            };
            stream_state.kind = promotion.from_kind;
            stream_state.pending_reader = promotion.pending_reader;
            stream_state.pending_read_roots = promotion.pending_roots;
            // Values queue should be empty (we failed before insertion).
            stream_state.values.clear();
            match promotion.from_kind {
              StreamKind::Strings => {
                stream_state.strings = promotion.strings;
                stream_state.buffered_string_len = promotion.buffered_string_len;
              }
              StreamKind::Bytes => {
                stream_state.byte_queue = promotion.byte_queue;
                stream_state.buffered_byte_len = promotion.buffered_byte_len;
              }
              _ => {}
            }
            Ok(())
          });

          // Ensure we don't leak the new chunk's persistent root if it was not inserted.
          scope.heap_mut().remove_root(chunk_root);
          Err(err)
        }
      }
    }
  }
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
      StreamKind::Bytes => stream_state.byte_queue.is_empty(),
      StreamKind::Strings => stream_state.strings.is_empty(),
      StreamKind::Values => stream_state.values.is_empty(),
      StreamKind::Uninitialized => true,
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
  let (pending, value_roots) = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get_mut(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError("ReadableStream error: invalid stream"))?;

    stream_state.state = StreamLifecycleState::Errored;
    stream_state.close_requested = true;
    stream_state.error_message = Some(error_message.clone());
    stream_state.byte_queue.clear();
    stream_state.buffered_byte_len = 0;
    stream_state.strings.clear();
    stream_state.buffered_string_len = 0;
    let value_roots: Vec<RootId> = stream_state.values.drain(..).collect();
    stream_state.init = None;

    let pending_reader = stream_state.pending_reader.take();
    if let Some(reader) = pending_reader {
      let roots = stream_state.pending_read_roots.take();
      return Ok((
        Some(PendingReadSettle {
          reader,
          roots,
          outcome: ReadOutcome::Error(error_message),
        }),
        value_roots,
      ));
    }

    debug_assert!(stream_state.pending_read_roots.is_none());
    Ok((None, value_roots))
  })?;

  for root_id in value_roots {
    scope.heap_mut().remove_root(root_id);
  }

  Ok(pending)
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
      .insert(
        WeakGcObject::from(obj),
        StreamState::new_from_bytes(bytes, DEFAULT_READABLE_STREAM_HIGH_WATER_MARK)?,
      );
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
      StreamState::new_lazy(Box::new(init), DEFAULT_READABLE_STREAM_HIGH_WATER_MARK),
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

  if let Some(realm_id) = vm.current_realm() {
    let gc_runs = heap.gc_runs();
    let Some(state) = registry.realms.get_mut(&realm_id) else {
      return false;
    };
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      let dead_streams: Vec<WeakGcObject> = state
        .streams
        .keys()
        .copied()
        .filter(|k| k.upgrade(heap).is_none())
        .collect();
      for k in dead_streams {
        if let Some(mut stream_state) = state.streams.remove(&k) {
          state
            .pending_value_root_cleanup
            .extend(stream_state.values.drain(..));
        }
      }
      state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }
    return state.streams.contains_key(&key);
  }

  // If we don't have a current realm (e.g. tests calling native handlers directly), fall back to
  // scanning all installed realm states. The number of realms is expected to be small.
  //
  // IMPORTANT: this fallback path must be read-only. The stream registry is global, and tests may
  // create multiple independent VMs/heaps in parallel. Sweeping another realm's weak refs using the
  // caller's heap can corrupt that realm state (WeakGcObject IDs are heap-local).
  for state in registry.realms.values() {
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

fn best_effort_reason_string(scope: &mut Scope<'_>, reason: Value, fallback: &str) -> Result<String, VmError> {
  // The `reason` can be any JS value. Converting it to a string may throw (e.g. Symbols, or custom
  // objects with throwing `toString`). For stream algorithms, we must not let those errors escape,
  // otherwise internal Promise reactions could throw and leave streams in inconsistent states.
  match scope.heap_mut().to_string(reason) {
    Ok(reason_string) => Ok(scope.heap().get_string(reason_string)?.to_utf8_lossy()),
    Err(err) if err.is_throw_completion() => Ok(fallback.to_string()),
    Err(err) => Err(err),
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
    WRITABLE_STREAM_LOCKED_KEY,
    Value::Bool(false),
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
  set_data_prop(
    scope,
    obj,
    WRITABLE_STREAM_LOCKED_KEY,
    Value::Bool(false),
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
  let locked = matches!(
    get_data_prop(scope, stream_obj, WRITABLE_STREAM_LOCKED_KEY)?,
    Value::Bool(true)
  );
  if locked {
    return Err(VmError::TypeError(
      "WritableStream.getWriter: stream is locked",
    ));
  }

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
  set_data_prop(
    scope,
    stream_obj,
    WRITABLE_STREAM_LOCKED_KEY,
    Value::Bool(true),
    false,
  )?;

  Ok(Value::Object(writer_obj))
}

fn writable_stream_locked_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let stream_obj = require_host_tag(
    scope,
    this,
    WRITABLE_STREAM_HOST_TAG,
    "WritableStream.locked: illegal invocation",
  )?;
  let locked = matches!(
    get_data_prop(scope, stream_obj, WRITABLE_STREAM_LOCKED_KEY)?,
    Value::Bool(true)
  );
  Ok(Value::Bool(locked))
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

fn writer_release_lock_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let writer_obj = require_writable_stream_writer(scope, this)?;
  let stream = match get_data_prop(scope, writer_obj, WRITABLE_STREAM_WRITER_STREAM_KEY)? {
    Value::Object(obj) => obj,
    _ => {
      // Already released.
      return Ok(Value::Undefined);
    }
  };
  set_data_prop(
    scope,
    stream,
    WRITABLE_STREAM_LOCKED_KEY,
    Value::Bool(false),
    false,
  )?;
  // Disconnect the writer from the stream.
  set_data_prop(
    scope,
    writer_obj,
    WRITABLE_STREAM_WRITER_STREAM_KEY,
    Value::Undefined,
    false,
  )?;
  Ok(Value::Undefined)
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

  let kind = with_realm_state_mut(vm, scope, callee, |state, _heap| {
    let stream_state = state
      .streams
      .get(&WeakGcObject::from(stream_obj))
      .ok_or(VmError::TypeError(
        "TransformStreamDefaultController.enqueue: invalid stream",
      ))?;
    Ok(stream_state.kind)
  })?;

  let pending = match kind {
    StreamKind::Values => enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?,
    StreamKind::Bytes => {
      if let Some(bytes) = extract_buffer_source_bytes(scope, chunk)? {
        enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?
      } else {
        enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?
      }
    }
    StreamKind::Strings => match chunk {
      Value::String(s) => {
        let code_units = scope.heap().get_string(s)?.as_code_units();
        let byte_len = utf8_len_from_utf16_units(code_units)?;
        if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
          return Err(VmError::TypeError("ReadableStream chunk too large"));
        }
        let chunk_string = utf16_units_to_utf8_string_lossy(code_units, byte_len)?;
        enqueue_string_into_readable_stream(vm, scope, callee, stream_obj, chunk_string)?
      }
      _ => enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?,
    },
    StreamKind::Uninitialized => {
      if let Some(bytes) = extract_buffer_source_bytes(scope, chunk)? {
        enqueue_bytes_into_readable_stream(vm, scope, callee, stream_obj, bytes)?
      } else if let Value::String(s) = chunk {
        let code_units = scope.heap().get_string(s)?.as_code_units();
        let byte_len = utf8_len_from_utf16_units(code_units)?;
        if byte_len > MAX_READABLE_STREAM_STRING_CHUNK_BYTES {
          return Err(VmError::TypeError("ReadableStream chunk too large"));
        }
        let chunk_string = utf16_units_to_utf8_string_lossy(code_units, byte_len)?;
        enqueue_string_into_readable_stream(vm, scope, callee, stream_obj, chunk_string)?
      } else {
        enqueue_value_into_readable_stream(vm, scope, callee, stream_obj, chunk)?
      }
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
  let msg = best_effort_reason_string(scope, reason, "TransformStream errored")?;

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
      Ok(value) => {
        // `transform()` may return a Promise (e.g. `async transform`). If it rejects, we must error
        // the readable side so pending reads don't hang forever.
        //
        // Spec shape: wrap in `PromiseResolve` and attach a rejection handler that calls
        // `controller.error(reason)` (modeled here via `error_readable_stream`) and then rethrows so
        // `writer.write()` is rejected with the same reason.
        let transform_promise = promise_resolve_with_host_and_hooks(vm, scope, host, hooks, value)?;
        let Value::Object(transform_promise_obj) = transform_promise else {
          return Err(VmError::InvariantViolation(
            "PromiseResolve must return an object",
          ));
        };

        let rejected_call_id = with_realm_state_mut(vm, scope, callee, |state, _heap| {
          Ok(state.transform_close_after_flush_rejected_call_id)
        })?;

        let realm_id = realm_id_for_binding_call(vm, scope.heap(), callee)?;
        let realm_slot = Value::Number(realm_id.to_raw() as f64);

        // Root stream + promise across callback allocation.
        let mut scope = scope.reborrow();
        scope.push_root(Value::Object(stream_obj))?;
        scope.push_root(transform_promise)?;

        let on_rejected_name = scope.alloc_string("TransformStream transform rejected")?;
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
          Value::Object(transform_promise_obj),
          None,
          Some(Value::Object(on_rejected)),
        )?;

        // `transform_promise` is not returned to user code, so mark it as handled to avoid
        // `unhandledrejection` if it rejects before `then` reactions run.
        mark_promise_handled(&mut scope, transform_promise)?;

        Ok(derived)
      }
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
      let data = scope.heap().uint8_array_data(obj)?;
      if data.len() > MAX_READABLE_STREAM_BYTE_CHUNK_BYTES {
        return Err(VmError::TypeError("ReadableStream chunk too large"));
      }
      let bytes = data.to_vec();
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

  let msg = best_effort_reason_string(scope, reason, "TransformStream transform rejected")?;
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
  let msg = best_effort_reason_string(scope, reason, "TransformStream aborted")?;

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

// === QueuingStrategy constructors (minimal stubs) ==============================

fn byte_length_queuing_strategy_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "ByteLengthQueuingStrategy constructor requires 'new'",
  ))
}

fn byte_length_queuing_strategy_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm, "ByteLengthQueuingStrategy requires intrinsics")?;

  let init_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(init_obj) = init_val else {
    return Err(VmError::TypeError(
      "ByteLengthQueuingStrategy constructor expects an options object",
    ));
  };

  // Root init object across property access/conversion.
  scope.push_root(Value::Object(init_obj))?;
  let high_water_mark_key = alloc_key(scope, "highWaterMark")?;
  let high_water_mark_val =
    vm.get_with_host_and_hooks(host, scope, hooks, init_obj, high_water_mark_key)?;
  let mut high_water_mark = scope.heap_mut().to_number(high_water_mark_val)?;
  if high_water_mark.is_nan() || high_water_mark < 0.0 {
    return Err(VmError::Throw(new_range_error(
      scope,
      intr,
      "The highWaterMark value is invalid.",
    )?));
  }
  // Canonicalize -0 to +0.
  if high_water_mark == 0.0 {
    high_water_mark = 0.0;
  }

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
      a: BYTE_LENGTH_QUEUING_STRATEGY_HOST_TAG,
      b: 0,
    },
  )?;

  let high_water_mark_key = alloc_key(scope, "highWaterMark")?;
  scope.define_property(
    obj,
    high_water_mark_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(high_water_mark),
        writable: false,
      },
    },
  )?;

  Ok(Value::Object(obj))
}

fn byte_length_queuing_strategy_size_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // WHATWG Streams: return ? GetV(chunk, "byteLength").
  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  let chunk_obj = scope.to_object(vm, host, hooks, chunk)?;
  scope.push_root(Value::Object(chunk_obj))?;
  let byte_length_key = alloc_key(scope, "byteLength")?;
  let byte_length_val =
    vm.get_with_host_and_hooks(host, scope, hooks, chunk_obj, byte_length_key)?;
  let n = scope.heap_mut().to_number(byte_length_val)?;
  Ok(Value::Number(n))
}

fn count_queuing_strategy_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "CountQueuingStrategy constructor requires 'new'",
  ))
}

fn count_queuing_strategy_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm, "CountQueuingStrategy requires intrinsics")?;

  let init_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(init_obj) = init_val else {
    return Err(VmError::TypeError(
      "CountQueuingStrategy constructor expects an options object",
    ));
  };

  // Root init object across property access/conversion.
  scope.push_root(Value::Object(init_obj))?;
  let high_water_mark_key = alloc_key(scope, "highWaterMark")?;
  let high_water_mark_val =
    vm.get_with_host_and_hooks(host, scope, hooks, init_obj, high_water_mark_key)?;
  let mut high_water_mark = scope.heap_mut().to_number(high_water_mark_val)?;
  if high_water_mark.is_nan() || high_water_mark < 0.0 {
    return Err(VmError::Throw(new_range_error(
      scope,
      intr,
      "The highWaterMark value is invalid.",
    )?));
  }
  if high_water_mark == 0.0 {
    high_water_mark = 0.0;
  }

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
      a: COUNT_QUEUING_STRATEGY_HOST_TAG,
      b: 0,
    },
  )?;

  let high_water_mark_key = alloc_key(scope, "highWaterMark")?;
  scope.define_property(
    obj,
    high_water_mark_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(high_water_mark),
        writable: false,
      },
    },
  )?;

  Ok(Value::Object(obj))
}

fn count_queuing_strategy_size_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Number(1.0))
}

pub(crate) fn readable_stream_is_locked(vm: &Vm, heap: &Heap, obj: GcObject) -> bool {
  let mut registry = registry().lock().unwrap_or_else(|err| err.into_inner());

  let key = WeakGcObject::from(obj);

  if let Some(realm_id) = vm.current_realm() {
    let gc_runs = heap.gc_runs();
    let Some(state) = registry.realms.get_mut(&realm_id) else {
      return false;
    };
    if gc_runs != state.last_gc_runs {
      state.last_gc_runs = gc_runs;
      let dead_streams: Vec<WeakGcObject> = state
        .streams
        .keys()
        .copied()
        .filter(|k| k.upgrade(heap).is_none())
        .collect();
      for k in dead_streams {
        if let Some(mut stream_state) = state.streams.remove(&k) {
          state
            .pending_value_root_cleanup
            .extend(stream_state.values.drain(..));
        }
      }
      state.readers.retain(|k, _| k.upgrade(heap).is_some());
    }
    return state.streams.get(&key).map_or(false, |s| s.locked);
  }

  // If we don't have a current realm (e.g. tests calling native handlers directly), fall back to
  // scanning all installed realm states. The number of realms is expected to be small.
  //
  // IMPORTANT: this fallback path must be read-only. See the comment in
  // `is_readable_stream_object` for details.
  for state in registry.realms.values() {
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

  let tee_call_id: NativeFunctionId = vm.register_native_call(readable_stream_tee_native)?;
  let tee_name = scope.alloc_string("tee")?;
  scope.push_root(Value::String(tee_name))?;
  let tee_fn = scope.alloc_native_function_with_slots(
    tee_call_id,
    None,
    tee_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(tee_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(tee_fn, Some(intr.function_prototype()))?;
  let tee_key = alloc_key(&mut scope, "tee")?;
  scope.define_property(
    stream_proto,
    tee_key,
    data_desc(Value::Object(tee_fn), true),
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

  let iterator_next_id: NativeFunctionId = vm.register_native_call(iterator_next_native)?;
  let iterator_next_fulfilled_id: NativeFunctionId =
    vm.register_native_call(iterator_next_fulfilled_native)?;
  let iterator_next_rejected_id: NativeFunctionId =
    vm.register_native_call(iterator_next_rejected_native)?;
  let iterator_return_id: NativeFunctionId = vm.register_native_call(iterator_return_native)?;
  let iterator_async_iterator_id: NativeFunctionId =
    vm.register_native_call(iterator_async_iterator_native)?;

  // `values({ preventCancel })` + `@@asyncIterator` (for `for await...of`).
  let values_call_id: NativeFunctionId = vm.register_native_call(readable_stream_values_native)?;
  let values_name = scope.alloc_string("values")?;
  scope.push_root(Value::String(values_name))?;
  let values_fn = scope.alloc_native_function_with_slots(
    values_call_id,
    None,
    values_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(values_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(values_fn, Some(intr.function_prototype()))?;
  let values_key = alloc_key(&mut scope, "values")?;
  scope.define_property(
    stream_proto,
    values_key,
    data_desc(Value::Object(values_fn), true),
  )?;
  let async_iter_key = PropertyKey::from_symbol(intr.well_known_symbols().async_iterator);
  scope.define_property(
    stream_proto,
    async_iter_key,
    data_desc(Value::Object(values_fn), true),
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

  let readable_stream_start_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_start_rejected_native)?;

  // tee pump callbacks.
  let readable_stream_tee_read_fulfilled_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_tee_read_fulfilled_native)?;
  let readable_stream_tee_read_rejected_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_tee_read_rejected_native)?;
  let readable_stream_tee_branch_cancel_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_tee_branch_cancel_native)?;

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

  let controller_desired_size_get_call_id: NativeFunctionId =
    vm.register_native_call(readable_stream_controller_desired_size_get_native)?;
  let controller_desired_size_get_name = scope.alloc_string("get desiredSize")?;
  scope.push_root(Value::String(controller_desired_size_get_name))?;
  let controller_desired_size_get_fn = scope.alloc_native_function_with_slots(
    controller_desired_size_get_call_id,
    None,
    controller_desired_size_get_name,
    0,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(controller_desired_size_get_fn))?;
  scope.heap_mut().object_set_prototype(
    controller_desired_size_get_fn,
    Some(intr.function_prototype()),
  )?;
  let controller_desired_size_key = alloc_key(&mut scope, "desiredSize")?;
  scope.define_property(
    readable_controller_proto,
    controller_desired_size_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(controller_desired_size_get_fn),
        set: Value::Undefined,
      },
    },
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

  let writer_release_lock_call_id: NativeFunctionId =
    vm.register_native_call(writer_release_lock_native)?;
  let writer_release_lock_name = scope.alloc_string("releaseLock")?;
  scope.push_root(Value::String(writer_release_lock_name))?;
  let writer_release_lock_fn = scope.alloc_native_function(
    writer_release_lock_call_id,
    None,
    writer_release_lock_name,
    0,
  )?;
  scope.push_root(Value::Object(writer_release_lock_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(writer_release_lock_fn, Some(intr.function_prototype()))?;
  let writer_release_lock_key = alloc_key(&mut scope, "releaseLock")?;
  scope.define_property(
    writer_proto,
    writer_release_lock_key,
    data_desc(Value::Object(writer_release_lock_fn), true),
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

  let writable_stream_locked_get_call_id: NativeFunctionId =
    vm.register_native_call(writable_stream_locked_get_native)?;
  let writable_stream_locked_get_name = scope.alloc_string("get locked")?;
  scope.push_root(Value::String(writable_stream_locked_get_name))?;
  let writable_stream_locked_get_fn = scope.alloc_native_function(
    writable_stream_locked_get_call_id,
    None,
    writable_stream_locked_get_name,
    0,
  )?;
  scope.push_root(Value::Object(writable_stream_locked_get_fn))?;
  scope.heap_mut().object_set_prototype(
    writable_stream_locked_get_fn,
    Some(intr.function_prototype()),
  )?;
  let writable_stream_locked_key = alloc_key(&mut scope, "locked")?;
  scope.define_property(
    writable_stream_proto,
    writable_stream_locked_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(writable_stream_locked_get_fn),
        set: Value::Undefined,
      },
    },
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

  // --- Queuing strategies ------------------------------------------------------
  // These are used by the full WHATWG Streams API to configure queuing/backpressure. Our stream
  // implementation does not currently use them, but many scripts expect the constructors to exist
  // and to expose the `highWaterMark`/`size` surface area.

  // ByteLengthQueuingStrategy
  let blqs_call_id: NativeFunctionId =
    vm.register_native_call(byte_length_queuing_strategy_ctor_call)?;
  let blqs_construct_id: NativeConstructId =
    vm.register_native_construct(byte_length_queuing_strategy_ctor_construct)?;
  let blqs_size_call_id: NativeFunctionId =
    vm.register_native_call(byte_length_queuing_strategy_size_native)?;

  let blqs_name = scope.alloc_string("ByteLengthQueuingStrategy")?;
  scope.push_root(Value::String(blqs_name))?;
  let blqs_ctor = scope.alloc_native_function_with_slots(
    blqs_call_id,
    Some(blqs_construct_id),
    blqs_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(blqs_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(blqs_ctor, Some(intr.function_prototype()))?;

  let blqs_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(blqs_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "ByteLengthQueuingStrategy constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(blqs_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(blqs_proto, Some(intr.object_prototype()))?;

  let blqs_tag_key = PropertyKey::from_symbol(to_string_tag);
  let blqs_tag_val = scope.alloc_string("ByteLengthQueuingStrategy")?;
  scope.push_root(Value::String(blqs_tag_val))?;
  scope.define_property(
    blqs_proto,
    blqs_tag_key,
    data_desc(Value::String(blqs_tag_val), false),
  )?;

  let blqs_size_name = scope.alloc_string("size")?;
  scope.push_root(Value::String(blqs_size_name))?;
  let blqs_size_fn = scope.alloc_native_function_with_slots(
    blqs_size_call_id,
    None,
    blqs_size_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(blqs_size_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(blqs_size_fn, Some(intr.function_prototype()))?;
  let blqs_size_key = alloc_key(&mut scope, "size")?;
  scope.define_property(
    blqs_proto,
    blqs_size_key,
    data_desc(Value::Object(blqs_size_fn), true),
  )?;

  let blqs_ctor_key = alloc_key(&mut scope, "ByteLengthQueuingStrategy")?;
  scope.define_property(
    global,
    blqs_ctor_key,
    data_desc(Value::Object(blqs_ctor), true),
  )?;

  // CountQueuingStrategy
  let cqs_call_id: NativeFunctionId = vm.register_native_call(count_queuing_strategy_ctor_call)?;
  let cqs_construct_id: NativeConstructId =
    vm.register_native_construct(count_queuing_strategy_ctor_construct)?;
  let cqs_size_call_id: NativeFunctionId =
    vm.register_native_call(count_queuing_strategy_size_native)?;

  let cqs_name = scope.alloc_string("CountQueuingStrategy")?;
  scope.push_root(Value::String(cqs_name))?;
  let cqs_ctor = scope.alloc_native_function_with_slots(
    cqs_call_id,
    Some(cqs_construct_id),
    cqs_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(cqs_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(cqs_ctor, Some(intr.function_prototype()))?;

  let cqs_proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(cqs_ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "CountQueuingStrategy constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(cqs_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(cqs_proto, Some(intr.object_prototype()))?;

  let cqs_tag_key = PropertyKey::from_symbol(to_string_tag);
  let cqs_tag_val = scope.alloc_string("CountQueuingStrategy")?;
  scope.push_root(Value::String(cqs_tag_val))?;
  scope.define_property(
    cqs_proto,
    cqs_tag_key,
    data_desc(Value::String(cqs_tag_val), false),
  )?;

  let cqs_size_name = scope.alloc_string("size")?;
  scope.push_root(Value::String(cqs_size_name))?;
  let cqs_size_fn = scope.alloc_native_function_with_slots(
    cqs_size_call_id,
    None,
    cqs_size_name,
    1,
    &[Value::Number(realm_id.to_raw() as f64)],
  )?;
  scope.push_root(Value::Object(cqs_size_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(cqs_size_fn, Some(intr.function_prototype()))?;
  let cqs_size_key = alloc_key(&mut scope, "size")?;
  scope.define_property(
    cqs_proto,
    cqs_size_key,
    data_desc(Value::Object(cqs_size_fn), true),
  )?;

  let cqs_ctor_key = alloc_key(&mut scope, "CountQueuingStrategy")?;
  scope.define_property(
    global,
    cqs_ctor_key,
    data_desc(Value::Object(cqs_ctor), true),
  )?;

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
      readable_stream_start_rejected_call_id,
      readable_stream_tee_read_fulfilled_call_id,
      readable_stream_tee_read_rejected_call_id,
      readable_stream_tee_branch_cancel_call_id,
      iterator_next_id,
      iterator_next_fulfilled_id,
      iterator_next_rejected_id,
      iterator_return_id,
      iterator_async_iterator_id,
      streams: HashMap::new(),
      readers: HashMap::new(),
      pending_value_root_cleanup: Vec::new(),
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
  fn readable_stream_get_reader_byob_throws_typeerror() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let threw = realm.exec_script(
      r#"(() => { try { new ReadableStream().getReader({ mode: 'byob' }); return false; } catch (e) { return e instanceof TypeError; } })()"#,
    )?;
    assert_eq!(threw, Value::Bool(true));

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

    // `enqueue()` with no args enqueues `undefined` (matches browser behaviour).
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
      assert!(matches!(value, Value::Undefined));
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
  fn readable_stream_controller_enqueue_with_no_args_enqueues_undefined() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) {
            controller.enqueue();
            controller.close();
          }
        });
        globalThis.reader = stream.getReader();
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
      PromiseState::Fulfilled
    );
    let Some(result1_val) = realm.heap().promise_result(read_p1_obj)? else {
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
      let done = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result1_obj, "value")?;
      assert!(matches!(value, Value::Undefined));
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_controller_can_enqueue_uint8array() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) {
            controller.enqueue(new Uint8Array([1, 2, 3]));
            controller.close();
          }
        });
        globalThis.reader = stream.getReader();
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
      PromiseState::Fulfilled
    );
    let Some(result1_val) = realm.heap().promise_result(read_p1_obj)? else {
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
      let done = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result1_obj, "value")?;
      let Value::Object(value_obj) = value else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      assert!(scope.heap().is_uint8_array_object(value_obj));
      assert_eq!(scope.heap().uint8_array_data(value_obj)?, &[1, 2, 3]);
    }

    let read_p2 = realm.exec_script("globalThis.readPromise2 = reader.read();")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
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
  fn readable_stream_byte_buffer_drops_consumed_chunks() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let realm_id = realm.realm().id();

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) { globalThis.controller = controller; }
        });
        globalThis.reader = stream.getReader();
        controller.enqueue(new Uint8Array([1, 2, 3]));
        controller.enqueue(new Uint8Array([4, 5, 6, 7]));
      "#,
    )?;

    let stream_val = realm.exec_script("stream")?;
    let Value::Object(stream_obj) = stream_val else {
      return Err(VmError::InvariantViolation("expected stream object"));
    };

    let buffered_before = {
      let registry = registry().lock().unwrap_or_else(|err| err.into_inner());
      let state = registry
        .realms
        .get(&realm_id)
        .ok_or(VmError::InvariantViolation("missing realm stream registry"))?;
      let stream_state = state
        .streams
        .get(&WeakGcObject::from(stream_obj))
        .ok_or(VmError::InvariantViolation("missing stream state"))?;
      stream_state.buffered_byte_len
    };

    assert_eq!(buffered_before, 7);

    let _ = realm.exec_script("globalThis.readPromise = reader.read();")?;

    let buffered_after = {
      let registry = registry().lock().unwrap_or_else(|err| err.into_inner());
      let state = registry
        .realms
        .get(&realm_id)
        .ok_or(VmError::InvariantViolation("missing realm stream registry"))?;
      let stream_state = state
        .streams
        .get(&WeakGcObject::from(stream_obj))
        .ok_or(VmError::InvariantViolation("missing stream state"))?;
      stream_state.buffered_byte_len
    };

    assert_eq!(buffered_after, 4);
    assert!(buffered_after < buffered_before);

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_controller_can_enqueue_object_values() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.obj = { a: 1 };
        globalThis.stream = new ReadableStream({
          start(controller) {
            controller.enqueue(globalThis.obj);
            controller.close();
          }
        });
        globalThis.reader = stream.getReader();
        globalThis.readPromise1 = reader.read();
        globalThis.readPromise2 = reader.read();
      "#,
    )?;

    let enqueued = realm.exec_script("obj")?;
    let Value::Object(enqueued_obj) = enqueued else {
      return Err(VmError::InvariantViolation("expected obj to be an object"));
    };

    // First read should return the exact object that was enqueued.
    let read_p1 = realm.exec_script("readPromise1")?;
    let Value::Object(read_p1_obj) = read_p1 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p1_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result1_val) = realm.heap().promise_result(read_p1_obj)? else {
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
      let done = read_result_prop(&mut scope, result1_obj, "done")?;
      assert_eq!(done, Value::Bool(false));
      let value = read_result_prop(&mut scope, result1_obj, "value")?;
      assert_eq!(value, Value::Object(enqueued_obj));

      let Value::Object(value_obj) = value else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      let a = read_result_prop(&mut scope, value_obj, "a")?;
      assert_eq!(a, Value::Number(1.0));
    }

    // Second read should be done.
    let read_p2 = realm.exec_script("readPromise2")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
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
    }

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
    realm.perform_microtask_checkpoint()?;

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
    realm.perform_microtask_checkpoint()?;

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
  fn readable_stream_pipe_to_releases_reader_lock_on_fulfill() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.source = new ReadableStream({
          start(controller) {
            controller.enqueue('x');
            controller.close();
          }
        });
        globalThis.pipePromise = source.pipeTo(new WritableStream());
      "#,
    )?;

    let pipe_promise = realm.exec_script("pipePromise")?;
    let Value::Object(pipe_promise_obj) = pipe_promise else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.pipeTo must return a Promise",
      ));
    };

    realm.perform_microtask_checkpoint()?;

    assert_eq!(
      realm.heap().promise_state(pipe_promise_obj)?,
      PromiseState::Fulfilled
    );
    assert_eq!(realm.exec_script("source.locked")?, Value::Bool(false));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_pipe_to_releases_reader_lock_on_reject() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.source = new ReadableStream({
          start(controller) {
            controller.enqueue('x');
            controller.close();
          }
        });
        globalThis.dest = new WritableStream({
          write() { throw new Error('fail'); }
        });
        globalThis.pipePromise = source.pipeTo(dest);
      "#,
    )?;

    let pipe_promise = realm.exec_script("pipePromise")?;
    let Value::Object(pipe_promise_obj) = pipe_promise else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.pipeTo must return a Promise",
      ));
    };

    realm.perform_microtask_checkpoint()?;

    assert_eq!(
      realm.heap().promise_state(pipe_promise_obj)?,
      PromiseState::Rejected
    );
    assert_eq!(realm.exec_script("source.locked")?, Value::Bool(false));

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_tee_splits_byte_stream_into_two_branches() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) {
            controller.enqueue('hi');
            controller.close();
          }
        }).pipeThrough(new TextEncoderStream());
        globalThis.branches = stream.tee();
        globalThis.branch0 = branches[0];
        globalThis.branch1 = branches[1];
        globalThis.lockedAfterTee = stream.locked;
      "#,
    )?;

    assert_eq!(realm.exec_script("lockedAfterTee")?, Value::Bool(true));
    assert_eq!(
      realm.exec_script("branch0 instanceof ReadableStream")?,
      Value::Bool(true)
    );
    assert_eq!(
      realm.exec_script("branch1 instanceof ReadableStream")?,
      Value::Bool(true)
    );

    let _ = realm.exec_script(
      r#"
        globalThis.reader0 = branch0.getReader();
        globalThis.reader1 = branch1.getReader();
        globalThis.p0_1 = reader0.read();
        globalThis.p1_1 = reader1.read();
      "#,
    )?;

    realm.perform_microtask_checkpoint()?;

    let p0_1 = realm.exec_script("p0_1")?;
    let p1_1 = realm.exec_script("p1_1")?;
    let (Value::Object(p0_1_obj), Value::Object(p1_1_obj)) = (p0_1, p1_1) else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(p0_1_obj)?,
      PromiseState::Fulfilled
    );
    assert_eq!(
      realm.heap().promise_state(p1_1_obj)?,
      PromiseState::Fulfilled
    );

    let Some(r0_val) = realm.heap().promise_result(p0_1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Some(r1_val) = realm.heap().promise_result(p1_1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let (Value::Object(r0_obj), Value::Object(r1_obj)) = (r0_val, r1_val) else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();

      let done0 = read_result_prop(&mut scope, r0_obj, "done")?;
      let done1 = read_result_prop(&mut scope, r1_obj, "done")?;
      assert_eq!(done0, Value::Bool(false));
      assert_eq!(done1, Value::Bool(false));

      let v0 = read_result_prop(&mut scope, r0_obj, "value")?;
      let v1 = read_result_prop(&mut scope, r1_obj, "value")?;
      let (Value::Object(v0_obj), Value::Object(v1_obj)) = (v0, v1) else {
        return Err(VmError::InvariantViolation(
          "read() result.value must be an object",
        ));
      };
      assert!(scope.heap().is_uint8_array_object(v0_obj));
      assert!(scope.heap().is_uint8_array_object(v1_obj));
      assert_eq!(scope.heap().uint8_array_data(v0_obj)?, b"hi");
      assert_eq!(scope.heap().uint8_array_data(v1_obj)?, b"hi");
    }

    let _ = realm.exec_script(
      r#"
        globalThis.p0_2 = reader0.read();
        globalThis.p1_2 = reader1.read();
      "#,
    )?;

    realm.perform_microtask_checkpoint()?;

    let p0_2 = realm.exec_script("p0_2")?;
    let p1_2 = realm.exec_script("p1_2")?;
    let (Value::Object(p0_2_obj), Value::Object(p1_2_obj)) = (p0_2, p1_2) else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(p0_2_obj)?,
      PromiseState::Fulfilled
    );
    assert_eq!(
      realm.heap().promise_state(p1_2_obj)?,
      PromiseState::Fulfilled
    );

    let Some(r0_done_val) = realm.heap().promise_result(p0_2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Some(r1_done_val) = realm.heap().promise_result(p1_2_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let (Value::Object(r0_done_obj), Value::Object(r1_done_obj)) = (r0_done_val, r1_done_val)
    else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };
    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();
      let done0 = read_result_prop(&mut scope, r0_done_obj, "done")?;
      let done1 = read_result_prop(&mut scope, r1_done_obj, "done")?;
      assert_eq!(done0, Value::Bool(true));
      assert_eq!(done1, Value::Bool(true));
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_tee_preserves_object_identity_for_value_chunks() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.o = { a: 1 };
        globalThis.rs = new ReadableStream({
          start(c) {
            c.enqueue(o);
            c.close();
          }
        });
        const [b0, b1] = rs.tee();
        globalThis.r0 = b0.getReader();
        globalThis.r1 = b1.getReader();
        globalThis.p0 = r0.read();
        globalThis.p1 = r1.read();
      "#,
    )?;

    realm.perform_microtask_checkpoint()?;

    let o = realm.exec_script("o")?;
    let Value::Object(o_obj) = o else {
      return Err(VmError::InvariantViolation("globalThis.o must be an object"));
    };

    let p0 = realm.exec_script("p0")?;
    let p1 = realm.exec_script("p1")?;
    let (Value::Object(p0_obj), Value::Object(p1_obj)) = (p0, p1) else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };

    assert_eq!(
      realm.heap().promise_state(p0_obj)?,
      PromiseState::Fulfilled
    );
    assert_eq!(
      realm.heap().promise_state(p1_obj)?,
      PromiseState::Fulfilled
    );

    let Some(r0_val) = realm.heap().promise_result(p0_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let Some(r1_val) = realm.heap().promise_result(p1_obj)? else {
      return Err(VmError::InvariantViolation("read() promise missing result"));
    };
    let (Value::Object(r0_obj), Value::Object(r1_obj)) = (r0_val, r1_val) else {
      return Err(VmError::InvariantViolation(
        "read() must resolve to an object",
      ));
    };

    {
      let heap = realm.heap_mut();
      let mut scope = heap.scope();

      let done0 = read_result_prop(&mut scope, r0_obj, "done")?;
      let done1 = read_result_prop(&mut scope, r1_obj, "done")?;
      assert_eq!(done0, Value::Bool(false));
      assert_eq!(done1, Value::Bool(false));

      let v0 = read_result_prop(&mut scope, r0_obj, "value")?;
      let v1 = read_result_prop(&mut scope, r1_obj, "value")?;

      assert_eq!(v0, Value::Object(o_obj));
      assert_eq!(v1, Value::Object(o_obj));
      assert_eq!(v0, v1);
    }

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_tee_branch_cancel_does_not_break_other_branch() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.stream = new ReadableStream({
          start(controller) {
            controller.enqueue('hi');
            controller.close();
          }
        }).pipeThrough(new TextEncoderStream());
        globalThis.branches = stream.tee();
        globalThis.branch0 = branches[0];
        globalThis.branch1 = branches[1];
        globalThis.cancelPromise = branch0.cancel();
      "#,
    )?;

    let cancel_promise = realm.exec_script("cancelPromise")?;
    let Value::Object(cancel_promise_obj) = cancel_promise else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.cancel must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(cancel_promise_obj)?,
      PromiseState::Fulfilled
    );

    let _ = realm.exec_script(
      r#"
        globalThis.reader1 = branch1.getReader();
        globalThis.readPromise1 = reader1.read();
      "#,
    )?;

    realm.perform_microtask_checkpoint()?;

    let read_p1 = realm.exec_script("readPromise1")?;
    let Value::Object(read_p1_obj) = read_p1 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(
      realm.heap().promise_state(read_p1_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result1_val) = realm.heap().promise_result(read_p1_obj)? else {
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

    let _ = realm.exec_script("globalThis.readPromise2 = reader1.read();")?;

    realm.perform_microtask_checkpoint()?;

    let read_p2 = realm.exec_script("readPromise2")?;
    let Value::Object(read_p2_obj) = read_p2 else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
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
      let done2 = read_result_prop(&mut scope, result2_obj, "done")?;
      assert_eq!(done2, Value::Bool(true));
      let value2 = read_result_prop(&mut scope, result2_obj, "value")?;
      assert!(matches!(value2, Value::Undefined));
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
  fn readable_stream_enqueuing_past_total_buffer_cap_errors_stream() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Use a moderate chunk size so the test doesn't allocate too much, but still requires multiple
    // enqueue calls to exceed the per-stream cap.
    let chunk_size = 1024 * 1024; // 1MiB
    let iterations = (MAX_READABLE_STREAM_QUEUED_BYTES / chunk_size) + 1;

    let script = format!(
      r#"
        globalThis.stream = new ReadableStream({{
          start(controller) {{ globalThis.controller = controller; }}
        }});
        globalThis.reader = stream.getReader();
        globalThis.chunk = new Uint8Array({chunk_size});
        for (let i = 0; i < {iterations}; i++) {{
          controller.enqueue(chunk);
        }}
      "#,
      chunk_size = chunk_size,
      iterations = iterations,
    );
    let _ = realm.exec_script(&script)?;

    let p = realm.exec_script("reader.read()")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Rejected);

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_enqueuing_past_total_chunk_cap_errors_stream() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    // Enqueue lots of empty chunks so total queued bytes stays low, but the queue length limit is
    // exceeded.
    let iterations = MAX_READABLE_STREAM_QUEUED_CHUNKS + 1;

    let script = format!(
      r#"
        globalThis.stream = new ReadableStream({{
          start(controller) {{ globalThis.controller = controller; }}
        }});
        globalThis.reader = stream.getReader();
        globalThis.chunk = new Uint8Array(0);
        for (let i = 0; i < {iterations}; i++) {{
          controller.enqueue(chunk);
        }}
      "#,
      iterations = iterations,
    );
    let _ = realm.exec_script(&script)?;

    let p = realm.exec_script("reader.read()")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.read must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Rejected);

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_cancel_rejects_with_stored_error_reason_when_errored() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.rs = new ReadableStream({ start(c){ globalThis.c=c; } });
        globalThis.r = rs.getReader();
        globalThis.err = {x: 1};
        c.error(err);
        globalThis.p = rs.cancel('ignored');
      "#,
    )?;

    let p = realm.exec_script("p")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.cancel must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Rejected);

    let Some(reason) = realm.heap().promise_result(p_obj)? else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.cancel promise missing rejection reason",
      ));
    };
    let err = realm.exec_script("err")?;
    assert_eq!(reason, err);

    realm.teardown();
    Ok(())
  }

  #[test]
  fn readable_stream_default_reader_cancel_rejects_with_stored_error_reason_when_errored(
  ) -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script(
      r#"
        globalThis.rs = new ReadableStream({ start(c){ globalThis.c=c; } });
        globalThis.r = rs.getReader();
        globalThis.err = {x: 1};
        c.error(err);
        globalThis.p = r.cancel('ignored');
      "#,
    )?;

    let p = realm.exec_script("p")?;
    let Value::Object(p_obj) = p else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.cancel must return a Promise",
      ));
    };
    assert_eq!(realm.heap().promise_state(p_obj)?, PromiseState::Rejected);

    let Some(reason) = realm.heap().promise_result(p_obj)? else {
      return Err(VmError::InvariantViolation(
        "ReadableStreamDefaultReader.cancel promise missing rejection reason",
      ));
    };
    let err = realm.exec_script("err")?;
    assert_eq!(reason, err);

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
      // Keep the heap borrow scoped so we can call back into `realm.exec_script` afterwards.
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
  fn writable_stream_locking_and_writer_release_lock() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let _ = realm.exec_script("globalThis.ws = new WritableStream();")?;
    let _ = realm.exec_script("globalThis.w1 = ws.getWriter();")?;

    let locked = realm.exec_script("ws.locked")?;
    assert_eq!(locked, Value::Bool(true));

    let second_get_writer_threw = realm.exec_script(
      "(() => { try { ws.getWriter(); return false; } catch (e) { return e instanceof TypeError; } })()",
    )?;
    assert_eq!(second_get_writer_threw, Value::Bool(true));

    let _ = realm.exec_script("w1.releaseLock();")?;

    let locked_after_release = realm.exec_script("ws.locked")?;
    assert_eq!(locked_after_release, Value::Bool(false));

    let get_writer_after_release_ok = realm.exec_script(
      "(() => { try { ws.getWriter(); return true; } catch (e) { return false; } })()",
    )?;
    assert_eq!(get_writer_after_release_ok, Value::Bool(true));

    let write_after_release_threw = realm.exec_script(
      "(() => { try { w1.write('x'); return false; } catch (e) { return e instanceof TypeError; } })()",
    )?;
    assert_eq!(write_after_release_threw, Value::Bool(true));

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
