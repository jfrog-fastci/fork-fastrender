//! Minimal `TextEncoder` / `TextDecoder` bindings for `Window` realms.
//!
//! These APIs are widely used by real-world scripts (analytics, polyfills, fetch helpers).
//! FastRender currently provides a UTF-8-only implementation backed by `vm-js` `ArrayBuffer` /
//! `Uint8Array` primitives.

use std::char;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use vm_js::{
  new_range_error, Heap, HostSlots, Intrinsics, NativeConstructId, NativeFunctionId,
  PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, WeakGcObject,
};

const TEXT_ENCODER_HOST_TAG: u64 = 0x5445_5854_454E_4344; // "TEXTENCD"
const TEXT_DECODER_HOST_TAG: u64 = 0x5445_5854_4445_4344; // "TEXTDECD"
const TEXT_ENCODER_STREAM_HOST_TAG: u64 = 0x5445_5354_454E_5354; // "TESTENST" (TextEncoderStream)
const TEXT_DECODER_STREAM_HOST_TAG: u64 = 0x5444_4543_5354_524D; // "TDECSTRM" (TextDecoderStream)
const TEXT_DECODER_STREAM_TRANSFORMER_HOST_TAG: u64 = 0x5444_5354_5241_4E53; // "TDSTRANS"

const TEXT_DECODER_FLAG_FATAL: u64 = 1 << 0;
const TEXT_DECODER_FLAG_IGNORE_BOM: u64 = 1 << 1;
const TEXT_DECODER_FLAGS_MASK: u64 = TEXT_DECODER_FLAG_FATAL | TEXT_DECODER_FLAG_IGNORE_BOM;

// Internal encoding identifiers stored in the TextDecoder host slots. We intentionally support a
// small set of encodings for now for determinism and boundedness.
const TEXT_DECODER_ENCODING_UTF8: u64 = 0;
const TEXT_DECODER_ENCODING_WINDOWS_1252: u64 = 1;
const TEXT_DECODER_ENCODING_UTF16LE: u64 = 2;
const TEXT_DECODER_ENCODING_UTF16BE: u64 = 3;

const TEXT_DECODER_ENCODING_SHIFT: u64 = 2;

#[derive(Debug)]
#[must_use = "Text encoding bindings are only valid while the returned TextEncodingBindings is kept alive"]
pub(crate) struct TextEncodingBindings {
  heap_key: usize,
}

#[derive(Default)]
struct TextEncodingContext {
  last_gc_runs: u64,
  decoders: HashMap<WeakGcObject, TextDecoderRuntimeState>,
}

struct TextDecoderRuntimeState {
  decoder: encoding_rs::Decoder,
}

static TEXT_ENCODING_CONTEXTS: OnceLock<Mutex<HashMap<usize, TextEncodingContext>>> = OnceLock::new();

fn text_encoding_contexts() -> &'static Mutex<HashMap<usize, TextEncodingContext>> {
  TEXT_ENCODING_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Drop for TextEncodingBindings {
  fn drop(&mut self) {
    if let Ok(mut map) = text_encoding_contexts().lock() {
      map.remove(&self.heap_key);
    }
  }
}

fn new_streaming_decoder(encoding: &'static Encoding, ignore_bom: bool) -> encoding_rs::Decoder {
  if ignore_bom {
    encoding.new_decoder_without_bom_handling()
  } else {
    encoding.new_decoder_with_bom_removal()
  }
}

fn with_text_encoding_context_mut<R>(
  heap: &Heap,
  f: impl FnOnce(&mut TextEncodingContext) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let heap_key = heap as *const Heap as usize;
  let mut lock = text_encoding_contexts()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let ctx = lock.get_mut(&heap_key).ok_or(VmError::Unimplemented(
    "Text encoding bindings not installed for this heap",
  ))?;

  let gc_runs = heap.gc_runs();
  if gc_runs != ctx.last_gc_runs {
    ctx.last_gc_runs = gc_runs;
    ctx.decoders.retain(|weak, _| weak.upgrade(heap).is_some());
  }

  f(ctx)
}

/// Upper bound on the number of UTF-16 code units accepted for a `TextDecoder` label.
///
/// This is a DoS resistance measure: we must not allocate a huge host string just to validate the
/// label. Real encoding labels are tiny.
const MAX_TEXT_DECODER_LABEL_CODE_UNITS: usize = 128;

/// Upper bound on the number of bytes accepted by `TextDecoder.decode`.
///
/// This is a DoS resistance measure: decoding allocates a host-side `Vec<u16>` before creating a JS
/// string.
const MAX_TEXT_DECODER_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on the number of UTF-8 bytes produced by `TextEncoder.encode`.
///
/// This bounds host-side allocations (`Vec<u8>`) before handing bytes into the VM heap as an
/// `ArrayBuffer`.
const MAX_TEXT_ENCODER_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on the number of UTF-8 bytes produced by a single `TextDecoderStream` transform.
///
/// This mirrors `MAX_TEXT_ENCODER_OUTPUT_BYTES` to avoid unbounded host allocations when decoding
/// attacker-controlled byte chunks.
const MAX_TEXT_DECODER_STREAM_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

fn buffer_source_bytes<'a>(
  heap: &'a Heap,
  value: Value,
  type_error_msg: &'static str,
) -> Result<&'a [u8], VmError> {
  match value {
    Value::Undefined => Ok(&[][..]),
    Value::Object(obj) => {
      if heap.is_array_buffer_object(obj) {
        heap.array_buffer_data(obj)
      } else if heap.is_typed_array_object(obj) {
        let (buffer_obj, byte_offset, byte_len) = heap.typed_array_view_bytes(obj)?;
        let data = heap.array_buffer_data(buffer_obj)?;
        let end = byte_offset.checked_add(byte_len).ok_or(VmError::InvariantViolation(
          "TypedArray byte offset overflow while decoding BufferSource",
        ))?;
        data.get(byte_offset..end).ok_or(VmError::InvariantViolation(
          "TypedArray view out of bounds while decoding BufferSource",
        ))
      } else if heap.is_data_view_object(obj) {
        let buffer_obj = heap.data_view_buffer(obj)?;
        let byte_offset = heap.data_view_byte_offset(obj)?;
        let byte_len = heap.data_view_byte_length(obj)?;
        let data = heap.array_buffer_data(buffer_obj)?;
        let end = byte_offset.checked_add(byte_len).ok_or(VmError::InvariantViolation(
          "DataView byte offset overflow while decoding BufferSource",
        ))?;
        data.get(byte_offset..end).ok_or(VmError::InvariantViolation(
          "DataView view out of bounds while decoding BufferSource",
        ))
      } else {
        Err(VmError::TypeError(type_error_msg))
      }
    }
    _ => Err(VmError::TypeError(type_error_msg)),
  }
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
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

fn is_ascii_whitespace_unit(unit: u16) -> bool {
  matches!(unit, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

fn trim_ascii_whitespace_units(mut units: &[u16]) -> &[u16] {
  while units.first().copied().is_some_and(is_ascii_whitespace_unit) {
    units = &units[1..];
  }
  while units.last().copied().is_some_and(is_ascii_whitespace_unit) {
    units = &units[..units.len().saturating_sub(1)];
  }
  units
}

fn encoding_from_label_code_units(code_units: &[u16]) -> Result<Option<&'static Encoding>, VmError> {
  let trimmed = trim_ascii_whitespace_units(code_units);
  if trimmed.is_empty() {
    return Ok(None);
  }
  if trimmed.len() > MAX_TEXT_DECODER_LABEL_CODE_UNITS {
    return Ok(None);
  }

  // Encoding labels are ASCII. Convert to lowercase bytes for `encoding_rs`.
  let mut bytes: Vec<u8> = Vec::new();
  bytes
    .try_reserve_exact(trimmed.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for &unit in trimmed {
    if unit > 0x7F {
      return Ok(None);
    }
    bytes.push((unit as u8).to_ascii_lowercase());
  }

  Ok(Encoding::for_label(bytes.as_slice()))
}

fn text_decoder_encoding_id(enc: &'static Encoding) -> Option<u64> {
  if std::ptr::eq(enc, UTF_8) {
    Some(TEXT_DECODER_ENCODING_UTF8)
  } else if std::ptr::eq(enc, WINDOWS_1252) {
    Some(TEXT_DECODER_ENCODING_WINDOWS_1252)
  } else if std::ptr::eq(enc, UTF_16LE) {
    Some(TEXT_DECODER_ENCODING_UTF16LE)
  } else if std::ptr::eq(enc, UTF_16BE) {
    Some(TEXT_DECODER_ENCODING_UTF16BE)
  } else {
    None
  }
}

fn text_decoder_encoding_from_id(id: u64) -> Option<&'static Encoding> {
  match id {
    TEXT_DECODER_ENCODING_UTF8 => Some(UTF_8),
    TEXT_DECODER_ENCODING_WINDOWS_1252 => Some(WINDOWS_1252),
    TEXT_DECODER_ENCODING_UTF16LE => Some(UTF_16LE),
    TEXT_DECODER_ENCODING_UTF16BE => Some(UTF_16BE),
    _ => None,
  }
}

fn text_decoder_encoding_label_from_id(id: u64) -> Option<&'static str> {
  match id {
    TEXT_DECODER_ENCODING_UTF8 => Some("utf-8"),
    TEXT_DECODER_ENCODING_WINDOWS_1252 => Some("windows-1252"),
    TEXT_DECODER_ENCODING_UTF16LE => Some("utf-16le"),
    TEXT_DECODER_ENCODING_UTF16BE => Some("utf-16be"),
    _ => None,
  }
}

fn text_decoder_pack_state(encoding_id: u64, flags: u64) -> u64 {
  debug_assert_eq!(
    flags & !TEXT_DECODER_FLAGS_MASK,
    0,
    "TextDecoder flags must fit in mask"
  );
  flags | (encoding_id << TEXT_DECODER_ENCODING_SHIFT)
}

fn text_decoder_state_flags(state: u64) -> u64 {
  state & TEXT_DECODER_FLAGS_MASK
}

fn text_decoder_state_encoding(state: u64) -> Result<&'static Encoding, VmError> {
  let id = state >> TEXT_DECODER_ENCODING_SHIFT;
  text_decoder_encoding_from_id(id).ok_or(VmError::InvariantViolation(
    "TextDecoder internal encoding id is invalid",
  ))
}

fn text_decoder_state_encoding_label(state: u64) -> Result<&'static str, VmError> {
  let id = state >> TEXT_DECODER_ENCODING_SHIFT;
  text_decoder_encoding_label_from_id(id).ok_or(VmError::InvariantViolation(
    "TextDecoder internal encoding id is invalid",
  ))
}

fn require_intrinsics(vm: &Vm) -> Result<Intrinsics, VmError> {
  vm.intrinsics().ok_or(VmError::Unimplemented(
    "TextEncoder/TextDecoder require intrinsics (create a Realm first)",
  ))
}

fn receiver_host_slots<'a>(
  scope: &'a Scope<'_>,
  obj: vm_js::GcObject,
) -> Result<HostSlots, VmError> {
  scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError(
      "incompatible receiver (missing host slots)",
    ))
}

fn require_text_encoder_receiver(
  scope: &Scope<'_>,
  this: Value,
) -> Result<vm_js::GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "TextEncoder.prototype.encode called on non-object",
    ));
  };

  let slots = receiver_host_slots(scope, obj)?;
  if slots.a != TEXT_ENCODER_HOST_TAG {
    return Err(VmError::TypeError(
      "TextEncoder.prototype.encode called on incompatible receiver",
    ));
  }
  Ok(obj)
}

fn require_text_encoder_stream_receiver(
  scope: &Scope<'_>,
  this: Value,
) -> Result<vm_js::GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "TextEncoderStream.prototype.encoding called on non-object",
    ));
  };
  let slots = receiver_host_slots(scope, obj)?;
  if slots.a != TEXT_ENCODER_STREAM_HOST_TAG {
    return Err(VmError::TypeError(
      "TextEncoderStream.prototype.encoding called on incompatible receiver",
    ));
  }
  Ok(obj)
}

fn require_text_decoder_stream_receiver(
  scope: &Scope<'_>,
  this: Value,
) -> Result<(vm_js::GcObject, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "TextDecoderStream: illegal invocation",
    ));
  };
  let slots = receiver_host_slots(scope, obj)?;
  if slots.a != TEXT_DECODER_STREAM_HOST_TAG {
    return Err(VmError::TypeError(
      "TextDecoderStream: illegal invocation",
    ));
  }
  Ok((obj, slots.b))
}

fn require_text_decoder_stream_transformer_receiver(
  scope: &Scope<'_>,
  this: Value,
) -> Result<(vm_js::GcObject, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "TextDecoderStream transformer: illegal invocation",
    ));
  };
  let slots = receiver_host_slots(scope, obj)?;
  if slots.a != TEXT_DECODER_STREAM_TRANSFORMER_HOST_TAG {
    return Err(VmError::TypeError(
      "TextDecoderStream transformer: illegal invocation",
    ));
  }
  Ok((obj, slots.b))
}

fn require_text_decoder_receiver(
  scope: &Scope<'_>,
  this: Value,
) -> Result<(vm_js::GcObject, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(
      "TextDecoder.prototype.decode called on non-object",
    ));
  };
  let slots = receiver_host_slots(scope, obj)?;
  if slots.a != TEXT_DECODER_HOST_TAG {
    return Err(VmError::TypeError(
      "TextDecoder.prototype.decode called on incompatible receiver",
    ));
  }
  Ok((obj, slots.b))
}

fn utf8_len_from_utf16_units(units: &[u16]) -> Result<usize, VmError> {
  let mut len: usize = 0;
  for unit in char::decode_utf16(units.iter().copied()) {
    let ch = unit.unwrap_or('\u{FFFD}');
    len = len.checked_add(ch.len_utf8()).ok_or(VmError::OutOfMemory)?;
  }
  Ok(len)
}

fn encode_utf16_units_to_utf8(units: &[u16], out: &mut Vec<u8>) {
  for unit in char::decode_utf16(units.iter().copied()) {
    let ch = unit.unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    out.extend_from_slice(encoded.as_bytes());
  }
}

fn text_encoder_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("TextEncoder constructor requires 'new'"))
}

// --- TextEncoderStream -------------------------------------------------------

const TEXT_ENCODER_STREAM_CTOR_SLOT_GLOBAL: usize = 0;
const TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_STREAM_KEY: usize = 1;
const TEXT_ENCODER_STREAM_CTOR_SLOT_READABLE_KEY: usize = 2;
const TEXT_ENCODER_STREAM_CTOR_SLOT_WRITABLE_KEY: usize = 3;
const TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_KEY: usize = 4;
const TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_FN: usize = 5;

const TEXT_ENCODER_STREAM_TRANSFORM_SLOT_ENQUEUE_KEY: usize = 0;

fn text_encoder_stream_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "TextEncoderStream constructor requires 'new'",
  ))
}

fn text_encoder_stream_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let slots = scope.heap().get_function_native_slots(callee)?;
  let global = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_GLOBAL)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing global slot",
      ))
    }
  };
  let transform_stream_key_s = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_STREAM_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing TransformStream key slot",
      ))
    }
  };
  let readable_key_s = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_READABLE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing readable key slot",
      ))
    }
  };
  let writable_key_s = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_WRITABLE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing writable key slot",
      ))
    }
  };
  let transform_key_s = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing transform key slot",
      ))
    }
  };
  let transform_fn = match slots
    .get(TEXT_ENCODER_STREAM_CTOR_SLOT_TRANSFORM_FN)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream constructor missing transform function slot",
      ))
    }
  };

  // Construct internal TransformStream.
  scope.push_root(Value::Object(global))?;
  let transform_stream_key = PropertyKey::from_string(transform_stream_key_s);
  let ts_ctor = vm.get_with_host_and_hooks(host, &mut scope, hooks, global, transform_stream_key)?;
  let Value::Object(_ts_ctor_obj) = ts_ctor else {
    return Err(VmError::TypeError("TransformStream is not available"));
  };
  scope.push_root(ts_ctor)?;

  let transformer_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(transformer_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(transformer_obj, Some(intr.object_prototype()))?;
  let transform_key = PropertyKey::from_string(transform_key_s);
  scope.define_property(
    transformer_obj,
    transform_key,
    data_desc(Value::Object(transform_fn)),
  )?;

  let ts = vm.construct_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    ts_ctor,
    &[Value::Object(transformer_obj)],
    ts_ctor,
  )?;
  let Value::Object(ts_obj) = ts else {
    return Err(VmError::InvariantViolation(
      "TransformStream constructor must return object",
    ));
  };
  scope.push_root(Value::Object(ts_obj))?;

  // Extract `{ readable, writable }` from the internal TransformStream.
  let readable_key = PropertyKey::from_string(readable_key_s);
  let writable_key = PropertyKey::from_string(writable_key_s);
  let readable_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, ts_obj, readable_key)?;
  scope.push_root(readable_val)?;
  let writable_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, ts_obj, writable_key)?;
  scope.push_root(writable_val)?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: TEXT_ENCODER_STREAM_HOST_TAG,
      b: 0,
    },
  )?;

  scope.define_property(obj, readable_key, read_only_data_desc(readable_val))?;
  scope.define_property(obj, writable_key, read_only_data_desc(writable_val))?;

  Ok(Value::Object(obj))
}

fn text_encoder_stream_get_encoding(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_text_encoder_stream_receiver(scope, this)?;
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.first().copied() {
    Some(Value::String(s)) => Ok(Value::String(s)),
    _ => Err(VmError::InvariantViolation(
      "TextEncoderStream encoding getter missing utf-8 slot",
    )),
  }
}

fn text_encoder_stream_transform(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  let controller = args.get(1).copied().unwrap_or(Value::Undefined);

  let Value::Object(controller_obj) = controller else {
    return Err(VmError::TypeError(
      "TextEncoderStream transform missing controller",
    ));
  };

  // `ToString` coercion can run JS and therefore allocate / trigger GC. Root the controller object
  // across encoding so it remains live even if GC runs during `chunk` coercion.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(controller_obj))?;

  // Encode input chunk to UTF-8 bytes (matching `TextEncoder.encode` default empty-string parameter
  // semantics).
  let view_obj = if matches!(chunk, Value::Undefined) {
    let ab = scope.alloc_array_buffer_from_u8_vec(Vec::new())?;
    scope.push_root(Value::Object(ab))?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
    let view = scope.alloc_uint8_array(ab, 0, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
    view
  } else {
    let s = match chunk {
      Value::String(s) => s,
      other => scope.to_string(vm, host, hooks, other)?,
    };
    let code_units = scope.heap().get_string(s)?.as_code_units();
    let byte_len = utf8_len_from_utf16_units(code_units)?;
    if byte_len > MAX_TEXT_ENCODER_OUTPUT_BYTES {
      return Err(VmError::TypeError("TextEncoderStream chunk output too large"));
    }

    let mut bytes: Vec<u8> = Vec::new();
    bytes
      .try_reserve_exact(byte_len)
      .map_err(|_| VmError::OutOfMemory)?;
    encode_utf16_units_to_utf8(code_units, &mut bytes);
    debug_assert_eq!(bytes.len(), byte_len);

    let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
    scope.push_root(Value::Object(ab))?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

    let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
    scope
      .heap_mut()
      .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
    view
  };

  // Root objects across the property lookup + enqueue call in case those operations trigger GC.
  scope.push_root(Value::Object(view_obj))?;

  // controller.enqueue(view)
  let slots = scope.heap().get_function_native_slots(callee)?;
  let enqueue_key_s = match slots
    .get(TEXT_ENCODER_STREAM_TRANSFORM_SLOT_ENQUEUE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextEncoderStream transform missing enqueue key slot",
      ))
    }
  };
  let enqueue_key = PropertyKey::from_string(enqueue_key_s);
  let enqueue_fn =
    vm.get_with_host_and_hooks(host, &mut scope, hooks, controller_obj, enqueue_key)?;
  vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    enqueue_fn,
    Value::Object(controller_obj),
    &[Value::Object(view_obj)],
  )?;

  Ok(Value::Undefined)
}

// --- TextDecoderStream -------------------------------------------------------

const TEXT_DECODER_STREAM_CTOR_SLOT_GLOBAL: usize = 0;
const TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_STREAM_KEY: usize = 1;
const TEXT_DECODER_STREAM_CTOR_SLOT_READABLE_KEY: usize = 2;
const TEXT_DECODER_STREAM_CTOR_SLOT_WRITABLE_KEY: usize = 3;
const TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_KEY: usize = 4;
const TEXT_DECODER_STREAM_CTOR_SLOT_FLUSH_KEY: usize = 5;
const TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_FN: usize = 6;
const TEXT_DECODER_STREAM_CTOR_SLOT_FLUSH_FN: usize = 7;

const TEXT_DECODER_STREAM_TRANSFORM_SLOT_ENQUEUE_KEY: usize = 0;

fn text_decoder_stream_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "TextDecoderStream constructor requires 'new'",
  ))
}

fn text_decoder_stream_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  // Validate encoding label (Encoding Standard: trim ASCII whitespace, ASCII case-insensitive).
  let mut encoding: &'static Encoding = UTF_8;
  if let Some(label_value) = args.get(0).copied() {
    if !matches!(label_value, Value::Undefined) {
      let label_string = match label_value {
        Value::String(s) => s,
        other => scope.to_string(vm, host, hooks, other)?,
      };
      let label_units = scope.heap().get_string(label_string)?.as_code_units();
      let enc = encoding_from_label_code_units(label_units)?;
      let Some(enc) = enc else {
        return Err(VmError::Throw(
          new_range_error(&mut scope, intr, "The encoding label provided is invalid.")?,
        ));
      };
      if text_decoder_encoding_id(enc).is_none() {
        return Err(VmError::Throw(
          new_range_error(&mut scope, intr, "The encoding label provided is invalid.")?,
        ));
      }
      encoding = enc;
    }
  }

  // Parse options: `{ fatal, ignoreBOM }`.
  let mut flags: u64 = 0;
  if let Some(options_value) = args.get(1).copied() {
    if let Value::Object(options_obj) = options_value {
      scope.push_root(Value::Object(options_obj))?;

      let fatal_key = alloc_key(&mut scope, "fatal")?;
      let fatal_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, fatal_key)?;
      if scope.heap().to_boolean(fatal_value)? {
        flags |= TEXT_DECODER_FLAG_FATAL;
      }

      let ignore_bom_key = alloc_key(&mut scope, "ignoreBOM")?;
      let ignore_bom_value =
        vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, ignore_bom_key)?;
      if scope.heap().to_boolean(ignore_bom_value)? {
        flags |= TEXT_DECODER_FLAG_IGNORE_BOM;
      }
    }
  }

  let encoding_id = text_decoder_encoding_id(encoding).ok_or(VmError::InvariantViolation(
    "TextDecoderStream constructed with unsupported encoding",
  ))?;
  let state = text_decoder_pack_state(encoding_id, flags);

  // Read constructor slots.
  let slots = scope.heap().get_function_native_slots(callee)?;
  let global = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_GLOBAL)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing global slot",
      ))
    }
  };
  let transform_stream_key_s = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_STREAM_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing TransformStream key slot",
      ))
    }
  };
  let readable_key_s = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_READABLE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing readable key slot",
      ))
    }
  };
  let writable_key_s = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_WRITABLE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing writable key slot",
      ))
    }
  };
  let transform_key_s = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing transform key slot",
      ))
    }
  };
  let flush_key_s = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_FLUSH_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing flush key slot",
      ))
    }
  };
  let transform_fn = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_TRANSFORM_FN)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing transform function slot",
      ))
    }
  };
  let flush_fn = match slots
    .get(TEXT_DECODER_STREAM_CTOR_SLOT_FLUSH_FN)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream constructor missing flush function slot",
      ))
    }
  };

  // Construct internal TransformStream.
  scope.push_root(Value::Object(global))?;
  let transform_stream_key = PropertyKey::from_string(transform_stream_key_s);
  let ts_ctor = vm.get_with_host_and_hooks(host, &mut scope, hooks, global, transform_stream_key)?;
  let Value::Object(_ts_ctor_obj) = ts_ctor else {
    return Err(VmError::TypeError("TransformStream is not available"));
  };
  scope.push_root(ts_ctor)?;

  let transformer_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(transformer_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(transformer_obj, Some(intr.object_prototype()))?;
  scope.heap_mut().object_set_host_slots(
    transformer_obj,
    HostSlots {
      a: TEXT_DECODER_STREAM_TRANSFORMER_HOST_TAG,
      b: state,
    },
  )?;

  let transform_key = PropertyKey::from_string(transform_key_s);
  scope.define_property(
    transformer_obj,
    transform_key,
    data_desc(Value::Object(transform_fn)),
  )?;
  let flush_key = PropertyKey::from_string(flush_key_s);
  scope.define_property(
    transformer_obj,
    flush_key,
    data_desc(Value::Object(flush_fn)),
  )?;

  let ts = vm.construct_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    ts_ctor,
    &[Value::Object(transformer_obj)],
    ts_ctor,
  )?;
  let Value::Object(ts_obj) = ts else {
    return Err(VmError::InvariantViolation(
      "TransformStream constructor must return object",
    ));
  };
  scope.push_root(Value::Object(ts_obj))?;

  // Initialize the streaming decoder state stored on the internal transformer object.
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let decoder = new_streaming_decoder(encoding, ignore_bom);
  with_text_encoding_context_mut(scope.heap(), |ctx| {
    ctx.decoders.insert(
      WeakGcObject::from(transformer_obj),
      TextDecoderRuntimeState { decoder },
    );
    Ok(())
  })?;

  // Extract `{ readable, writable }` from the internal TransformStream.
  let readable_key = PropertyKey::from_string(readable_key_s);
  let writable_key = PropertyKey::from_string(writable_key_s);
  let readable_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, ts_obj, readable_key)?;
  scope.push_root(readable_val)?;
  let writable_val = vm.get_with_host_and_hooks(host, &mut scope, hooks, ts_obj, writable_key)?;
  scope.push_root(writable_val)?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: TEXT_DECODER_STREAM_HOST_TAG,
      b: state,
    },
  )?;

  scope.define_property(obj, readable_key, read_only_data_desc(readable_val))?;
  scope.define_property(obj, writable_key, read_only_data_desc(writable_val))?;

  Ok(Value::Object(obj))
}

fn text_decoder_stream_get_encoding(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_stream_receiver(scope, this)?;
  let label = text_decoder_state_encoding_label(state)?;
  let s = scope.alloc_string(label)?;
  Ok(Value::String(s))
}

fn text_decoder_stream_get_fatal(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_stream_receiver(scope, this)?;
  Ok(Value::Bool(
    (text_decoder_state_flags(state) & TEXT_DECODER_FLAG_FATAL) != 0,
  ))
}

fn text_decoder_stream_get_ignore_bom(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_stream_receiver(scope, this)?;
  Ok(Value::Bool(
    (text_decoder_state_flags(state) & TEXT_DECODER_FLAG_IGNORE_BOM) != 0,
  ))
}

fn text_decoder_stream_transform(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (transformer_obj, state) = require_text_decoder_stream_transformer_receiver(scope, this)?;
  let flags = text_decoder_state_flags(state);
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let fatal = (flags & TEXT_DECODER_FLAG_FATAL) != 0;
  let encoding = text_decoder_state_encoding(state)?;

  let chunk = args.get(0).copied().unwrap_or(Value::Undefined);
  let controller = args.get(1).copied().unwrap_or(Value::Undefined);

  let Value::Object(controller_obj) = controller else {
    return Err(VmError::TypeError(
      "TextDecoderStream transform missing controller",
    ));
  };

  let heap = scope.heap();
  let data = buffer_source_bytes(
    heap,
    chunk,
    "TextDecoderStream expects an ArrayBuffer, TypedArray, or DataView",
  )?;

  if data.len() > MAX_TEXT_DECODER_INPUT_BYTES {
    return Err(VmError::TypeError("TextDecoderStream chunk too large"));
  }

  let decoded = with_text_encoding_context_mut(heap, |ctx| {
    use encoding_rs::CoderResult;

    let key = WeakGcObject::from(transformer_obj);
    let state = ctx.decoders.entry(key).or_insert_with(|| TextDecoderRuntimeState {
      decoder: new_streaming_decoder(encoding, ignore_bom),
    });

    let mut out = String::new();
    out
      .try_reserve_exact(
        data
          .len()
          .saturating_mul(3)
          .min(MAX_TEXT_DECODER_STREAM_OUTPUT_BYTES),
      )
      .map_err(|_| VmError::OutOfMemory)?;

    let mut had_errors = false;
    let mut src = data;
    loop {
      let (result, read, errors) = state.decoder.decode_to_string(src, &mut out, false);
      had_errors |= errors;
      if read > src.len() {
        return Err(VmError::InvariantViolation(
          "TextDecoderStream transform consumed more bytes than available",
        ));
      }
      src = &src[read..];

      match result {
        CoderResult::InputEmpty => break,
        CoderResult::OutputFull => {
          let remaining = MAX_TEXT_DECODER_STREAM_OUTPUT_BYTES.saturating_sub(out.len());
          if remaining == 0 {
            return Err(VmError::TypeError("TextDecoderStream chunk output too large"));
          }
          // Grow the output buffer in bounded increments.
          let grow_by = remaining.min(1024);
          out
            .try_reserve_exact(grow_by)
            .map_err(|_| VmError::OutOfMemory)?;
        }
      }
    }

    if fatal && had_errors {
      // Reset decoder state on error so subsequent operations (if any) start fresh.
      state.decoder = new_streaming_decoder(encoding, ignore_bom);
      return Err(VmError::TypeError(
        "The encoded data was not valid for the specified encoding",
      ));
    }

    Ok(out)
  })?;

  if decoded.is_empty() {
    return Ok(Value::Undefined);
  }

  let out_s = scope.alloc_string(&decoded)?;

  // Root objects across the property lookup + enqueue call in case those operations trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(controller_obj))?;
  scope.push_root(Value::String(out_s))?;

  // controller.enqueue(decoded)
  let slots = scope.heap().get_function_native_slots(callee)?;
  let enqueue_key_s = match slots
    .get(TEXT_DECODER_STREAM_TRANSFORM_SLOT_ENQUEUE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream transform missing enqueue key slot",
      ))
    }
  };
  let enqueue_key = PropertyKey::from_string(enqueue_key_s);
  let enqueue_fn =
    vm.get_with_host_and_hooks(host, &mut scope, hooks, controller_obj, enqueue_key)?;
  vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    enqueue_fn,
    Value::Object(controller_obj),
    &[Value::String(out_s)],
  )?;

  Ok(Value::Undefined)
}

fn text_decoder_stream_flush(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (transformer_obj, state) = require_text_decoder_stream_transformer_receiver(scope, this)?;
  let flags = text_decoder_state_flags(state);
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let fatal = (flags & TEXT_DECODER_FLAG_FATAL) != 0;
  let encoding = text_decoder_state_encoding(state)?;

  let controller = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(controller_obj) = controller else {
    return Err(VmError::TypeError("TextDecoderStream flush missing controller"));
  };

  let heap = scope.heap();
  let decoded = with_text_encoding_context_mut(heap, |ctx| {
    use encoding_rs::CoderResult;

    let key = WeakGcObject::from(transformer_obj);
    let state = ctx.decoders.entry(key).or_insert_with(|| TextDecoderRuntimeState {
      decoder: new_streaming_decoder(encoding, ignore_bom),
    });

    let mut out = String::new();
    out
      .try_reserve_exact(1024.min(MAX_TEXT_DECODER_STREAM_OUTPUT_BYTES))
      .map_err(|_| VmError::OutOfMemory)?;

    let mut had_errors = false;
    let mut src: &[u8] = &[];
    loop {
      let (result, read, errors) = state.decoder.decode_to_string(src, &mut out, true);
      had_errors |= errors;
      if read > src.len() {
        return Err(VmError::InvariantViolation(
          "TextDecoderStream flush consumed more bytes than available",
        ));
      }
      src = &src[read..];
      match result {
        CoderResult::InputEmpty => break,
        CoderResult::OutputFull => {
          let remaining = MAX_TEXT_DECODER_STREAM_OUTPUT_BYTES.saturating_sub(out.len());
          if remaining == 0 {
            return Err(VmError::TypeError("TextDecoderStream chunk output too large"));
          }
          let grow_by = remaining.min(1024);
          out
            .try_reserve_exact(grow_by)
            .map_err(|_| VmError::OutOfMemory)?;
        }
      }
    }

    if fatal && had_errors {
      state.decoder = new_streaming_decoder(encoding, ignore_bom);
      return Err(VmError::TypeError(
        "The encoded data was not valid for the specified encoding",
      ));
    }

    // Reset decoder state to release buffered bytes/code points after close.
    state.decoder = new_streaming_decoder(encoding, ignore_bom);

    Ok(out)
  })?;

  if decoded.is_empty() {
    return Ok(Value::Undefined);
  }

  let out_s = scope.alloc_string(&decoded)?;

  // Root objects across the property lookup + enqueue call in case those operations trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(controller_obj))?;
  scope.push_root(Value::String(out_s))?;

  // controller.enqueue(decoded)
  let slots = scope.heap().get_function_native_slots(callee)?;
  let enqueue_key_s = match slots
    .get(TEXT_DECODER_STREAM_TRANSFORM_SLOT_ENQUEUE_KEY)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "TextDecoderStream flush missing enqueue key slot",
      ))
    }
  };
  let enqueue_key = PropertyKey::from_string(enqueue_key_s);
  let enqueue_fn =
    vm.get_with_host_and_hooks(host, &mut scope, hooks, controller_obj, enqueue_key)?;
  vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    enqueue_fn,
    Value::Object(controller_obj),
    &[Value::String(out_s)],
  )?;

  Ok(Value::Undefined)
}

fn text_encoder_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: TEXT_ENCODER_HOST_TAG,
      b: 0,
    },
  )?;
  Ok(Value::Object(obj))
}

fn text_decoder_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("TextDecoder constructor requires 'new'"))
}

fn text_decoder_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  // Validate encoding label (Encoding Standard: trim ASCII whitespace, ASCII case-insensitive).
  let mut encoding: &'static Encoding = UTF_8;
  if let Some(label_value) = args.get(0).copied() {
    if !matches!(label_value, Value::Undefined) {
      let label_string = match label_value {
        Value::String(s) => s,
        other => scope.to_string(vm, host, hooks, other)?,
      };
      let label_units = scope.heap().get_string(label_string)?.as_code_units();
      let enc = encoding_from_label_code_units(label_units)?;
      let Some(enc) = enc else {
        return Err(VmError::Throw(
          new_range_error(&mut scope, intr, "The encoding label provided is invalid.")?,
        ));
      };
      if text_decoder_encoding_id(enc).is_none() {
        return Err(VmError::Throw(
          new_range_error(&mut scope, intr, "The encoding label provided is invalid.")?,
        ));
      }
      encoding = enc;
    }
  }

  // Parse options: `{ fatal, ignoreBOM }`.
  let mut flags: u64 = 0;
  if let Some(options_value) = args.get(1).copied() {
    if let Value::Object(options_obj) = options_value {
      scope.push_root(Value::Object(options_obj))?;

      let fatal_key = alloc_key(&mut scope, "fatal")?;
      let fatal_value =
        vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, fatal_key)?;
      if scope.heap().to_boolean(fatal_value)? {
        flags |= TEXT_DECODER_FLAG_FATAL;
      }

      let ignore_bom_key = alloc_key(&mut scope, "ignoreBOM")?;
      let ignore_bom_value =
        vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, ignore_bom_key)?;
      if scope.heap().to_boolean(ignore_bom_value)? {
        flags |= TEXT_DECODER_FLAG_IGNORE_BOM;
      }
    }
  }

  let encoding_id = text_decoder_encoding_id(encoding).ok_or(VmError::InvariantViolation(
    "TextDecoder constructed with unsupported encoding",
  ))?;
  let state = text_decoder_pack_state(encoding_id, flags);

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope
    .heap_mut()
    .object_set_host_slots(obj, HostSlots { a: TEXT_DECODER_HOST_TAG, b: state })?;

  // Initialize the per-instance streaming decoder state used by `TextDecoder.decode(.., {stream:true})`.
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let decoder = new_streaming_decoder(encoding, ignore_bom);
  with_text_encoding_context_mut(scope.heap(), |ctx| {
    ctx
      .decoders
      .insert(WeakGcObject::from(obj), TextDecoderRuntimeState { decoder });
    Ok(())
  })?;

  Ok(Value::Object(obj))
}

fn text_encoder_get_encoding(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_text_encoder_receiver(scope, this)?;
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.first().copied() {
    Some(Value::String(s)) => Ok(Value::String(s)),
    _ => Err(VmError::InvariantViolation(
      "TextEncoder encoding getter missing utf-8 slot",
    )),
  }
}

fn text_decoder_get_encoding(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_receiver(scope, this)?;
  let label = text_decoder_state_encoding_label(state)?;
  let s = scope.alloc_string(label)?;
  Ok(Value::String(s))
}

fn text_decoder_get_fatal(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_receiver(scope, this)?;
  Ok(Value::Bool(
    (text_decoder_state_flags(state) & TEXT_DECODER_FLAG_FATAL) != 0,
  ))
}

fn text_decoder_get_ignore_bom(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, state) = require_text_decoder_receiver(scope, this)?;
  Ok(Value::Bool(
    (text_decoder_state_flags(state) & TEXT_DECODER_FLAG_IGNORE_BOM) != 0,
  ))
}

fn text_encoder_encode(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_text_encoder_receiver(scope, this)?;

  let intr = require_intrinsics(vm)?;

  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(input, Value::Undefined) {
    // Fast path for the default empty-string parameter value.
    let ab = scope.alloc_array_buffer_from_u8_vec(Vec::new())?;
    scope.push_root(Value::Object(ab))?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

    let view = scope.alloc_uint8_array(ab, 0, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
    return Ok(Value::Object(view));
  }

  let s = match input {
    Value::String(s) => s,
    other => scope.to_string(vm, host, hooks, other)?,
  };
  let code_units = scope.heap().get_string(s)?.as_code_units();

  let byte_len = utf8_len_from_utf16_units(code_units)?;
  if byte_len > MAX_TEXT_ENCODER_OUTPUT_BYTES {
    return Err(VmError::TypeError("TextEncoder output too large"));
  }

  let mut bytes: Vec<u8> = Vec::new();
  bytes
    .try_reserve_exact(byte_len)
    .map_err(|_| VmError::OutOfMemory)?;
  encode_utf16_units_to_utf8(code_units, &mut bytes);

  debug_assert_eq!(bytes.len(), byte_len);

  let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
  scope.push_root(Value::Object(ab))?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;

  let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;

  Ok(Value::Object(view))
}

fn text_encoder_encode_into(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_text_encoder_receiver(scope, this)?;

  let intr = require_intrinsics(vm)?;

  let source = args.get(0).copied().unwrap_or(Value::Undefined);
  let destination = args.get(1).copied().ok_or(VmError::TypeError(
    "TextEncoder.encodeInto expects a Uint8Array destination",
  ))?;

  let dest_obj = match destination {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::TypeError(
        "TextEncoder.encodeInto expects a Uint8Array destination",
      ))
    }
  };
  if !scope.heap().is_uint8_array_object(dest_obj) {
    return Err(VmError::TypeError(
      "TextEncoder.encodeInto expects a Uint8Array destination",
    ));
  }

  let dest_len: usize = {
    // Borrow the heap immutably only long enough to read the view length; we will mutate the
    // backing ArrayBuffer during encoding.
    scope.heap().uint8_array_data(dest_obj)?.len()
  };
  if dest_len > MAX_TEXT_ENCODER_OUTPUT_BYTES {
    return Err(VmError::TypeError("TextEncoder destination too large"));
  }

  // `encodeInto` uses a default empty-string parameter value.
  let source_string = if matches!(source, Value::Undefined) {
    None
  } else {
    Some(match source {
      Value::String(s) => s,
      other => {
        // `ToString` can allocate and trigger GC (and may invoke user code), so ensure we keep the
        // destination view alive across coercion.
        let mut scope = scope.reborrow();
        scope.push_root(Value::Object(dest_obj))?;
        scope.to_string(vm, host, hooks, other)?
      }
    })
  };

  let (source_handle, source_len_units): (Option<vm_js::GcString>, usize) = match source_string {
    None => (None, 0),
    Some(s) => (Some(s), scope.heap().get_string(s)?.len_code_units()),
  };

  let mut read_units: usize = 0;
  let mut written_bytes: usize = 0;

  if let Some(source_handle) = source_handle {
    while read_units < source_len_units && written_bytes < dest_len {
      let (u0, u1) = {
        let units = scope.heap().get_string(source_handle)?.as_code_units();
        let u0 = units
          .get(read_units)
          .copied()
          .ok_or(VmError::InvariantViolation(
            "TextEncoder.encodeInto source index out of bounds",
          ))?;
        let u1 = units.get(read_units + 1).copied();
        (u0, u1)
      };

      let is_high = (0xD800..=0xDBFF).contains(&u0);
      let is_low = (0xDC00..=0xDFFF).contains(&u0);

      let (ch, consumed_units) = if is_high {
        if let Some(u1) = u1 {
          if (0xDC00..=0xDFFF).contains(&u1) {
            let high = (u0 as u32) - 0xD800;
            let low = (u1 as u32) - 0xDC00;
            let cp = 0x10000 + ((high << 10) | low);
            (char::from_u32(cp).unwrap_or('\u{FFFD}'), 2)
          } else {
            ('\u{FFFD}', 1)
          }
        } else {
          ('\u{FFFD}', 1)
        }
      } else if is_low {
        ('\u{FFFD}', 1)
      } else {
        (char::from_u32(u0 as u32).unwrap_or('\u{FFFD}'), 1)
      };

      let mut buf = [0u8; 4];
      let encoded = ch.encode_utf8(&mut buf);
      if written_bytes + encoded.len() > dest_len {
        break;
      }
      let wrote =
        scope
          .heap_mut()
          .uint8_array_write(dest_obj, written_bytes, encoded.as_bytes())?;
      debug_assert_eq!(
        wrote,
        encoded.len(),
        "uint8_array_write should write the full chunk when it fits"
      );

      written_bytes += wrote;
      read_units += consumed_units;
    }
  }

  // Return `{ read, written }`.
  let result = scope.alloc_object()?;
  scope.push_root(Value::Object(result))?;
  scope
    .heap_mut()
    .object_set_prototype(result, Some(intr.object_prototype()))?;

  let data_desc = |value| PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };

  let read_key = alloc_key(scope, "read")?;
  let written_key = alloc_key(scope, "written")?;
  scope.define_property(
    result,
    read_key,
    data_desc(Value::Number(read_units as f64)),
  )?;
  scope.define_property(
    result,
    written_key,
    data_desc(Value::Number(written_bytes as f64)),
  )?;

  Ok(Value::Object(result))
}

fn text_decoder_decode(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (obj, state) = require_text_decoder_receiver(scope, this)?;
  let flags = text_decoder_state_flags(state);
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let fatal = (flags & TEXT_DECODER_FLAG_FATAL) != 0;
  let encoding = text_decoder_state_encoding(state)?;

  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let options = args.get(1).copied().unwrap_or(Value::Undefined);

  // WebIDL `TextDecodeOptions` `{ stream }`.
  let stream = match options {
    Value::Object(options_obj) => {
      // Root the options object while retrieving the property.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(options_obj))?;
      let stream_key = alloc_key(&mut scope, "stream")?;
      let stream_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, stream_key)?;
      scope.heap().to_boolean(stream_value)?
    }
    _ => false,
  };

  let heap = scope.heap();
  let data = buffer_source_bytes(
    heap,
    input,
    "TextDecoder.decode expects an ArrayBuffer, TypedArray, or DataView",
  )?;

  if data.len() > MAX_TEXT_DECODER_INPUT_BYTES {
    return Err(VmError::TypeError("TextDecoder input too large"));
  }

  let decoded = with_text_encoding_context_mut(heap, |ctx| {
    use encoding_rs::CoderResult;

    // If runtime state is missing (e.g. due to unexpected registry mutation), recreate a decoder
    // so `TextDecoder.decode` still functions.
    let key = WeakGcObject::from(obj);
    let state = ctx.decoders.entry(key).or_insert_with(|| TextDecoderRuntimeState {
      decoder: new_streaming_decoder(encoding, ignore_bom),
    });

    let last = !stream;

    // Decode to a host `String`, then allocate a JS string.
    //
    // Reserve a pessimistic hint: the output byte length should be at most a small multiple of the
    // input length (U+FFFD replacement is 3 bytes). Keep it bounded by input size.
    let mut out = String::new();
    out
      .try_reserve(data.len().saturating_mul(3).min(MAX_TEXT_DECODER_INPUT_BYTES))
      .map_err(|_| VmError::OutOfMemory)?;

    let mut had_errors = false;
    let mut src = data;

    loop {
      let (result, read, errors) = state.decoder.decode_to_string(src, &mut out, last);
      had_errors |= errors;
      if read > src.len() {
        return Err(VmError::InvariantViolation(
          "TextDecoder.decode consumed more bytes than available",
        ));
      }
      src = &src[read..];

      match result {
        CoderResult::InputEmpty => break,
        CoderResult::OutputFull => {
          // `decode_to_string` should generally grow the String, but be defensive in case the
          // implementation reports output saturation.
          out.try_reserve(1024).map_err(|_| VmError::OutOfMemory)?;
          continue;
        }
      }
    }

    if fatal && had_errors {
      // Reset decoder state on error so subsequent calls start fresh.
      state.decoder = new_streaming_decoder(encoding, ignore_bom);
      return Err(VmError::TypeError(
        "The encoded data was not valid for the specified encoding",
      ));
    }

    // `stream: false` flushes and resets the decoder for the next call.
    if last {
      state.decoder = new_streaming_decoder(encoding, ignore_bom);
    }

    Ok(out)
  })?;

  let out = scope.alloc_string(&decoded)?;
  Ok(Value::String(out))
}

pub(crate) fn install_window_text_encoding_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut vm_js::Heap,
) -> Result<TextEncodingBindings, VmError> {
  let heap_key = heap as *const vm_js::Heap as usize;
  {
    let mut map = text_encoding_contexts()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    if map.contains_key(&heap_key) {
      return Err(VmError::Unimplemented(
        "install_window_text_encoding_bindings called more than once for the same heap",
      ));
    }
    map.insert(
      heap_key,
      TextEncodingContext {
        last_gc_runs: heap.gc_runs(),
        decoders: HashMap::new(),
      },
    );
  }

  let result = (|| -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- TextEncoder -----------------------------------------------------------
  let te_call_id: NativeFunctionId = vm.register_native_call(text_encoder_call)?;
  let te_construct_id: NativeConstructId = vm.register_native_construct(text_encoder_construct)?;
  let utf8_s = scope.alloc_string("utf-8")?;
  scope.push_root(Value::String(utf8_s))?;

  let te_name_s = scope.alloc_string("TextEncoder")?;
  scope.push_root(Value::String(te_name_s))?;
  let te_ctor = scope.alloc_native_function(te_call_id, Some(te_construct_id), te_name_s, 0)?;
  scope.push_root(Value::Object(te_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(te_ctor, Some(intr.function_prototype()))?;

  let te_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(te_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TextEncoder constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(te_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(te_proto, Some(intr.object_prototype()))?;

  let te_encode_call_id: NativeFunctionId = vm.register_native_call(text_encoder_encode)?;
  let te_encode_name_s = scope.alloc_string("encode")?;
  scope.push_root(Value::String(te_encode_name_s))?;
  let te_encode_fn = scope.alloc_native_function(te_encode_call_id, None, te_encode_name_s, 1)?;
  scope.push_root(Value::Object(te_encode_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(te_encode_fn, Some(intr.function_prototype()))?;

  let encode_key = alloc_key(&mut scope, "encode")?;
  scope.define_property(te_proto, encode_key, data_desc(Value::Object(te_encode_fn)))?;

  let te_encode_into_call_id: NativeFunctionId =
    vm.register_native_call(text_encoder_encode_into)?;
  let te_encode_into_name_s = scope.alloc_string("encodeInto")?;
  scope.push_root(Value::String(te_encode_into_name_s))?;
  let te_encode_into_fn =
    scope.alloc_native_function(te_encode_into_call_id, None, te_encode_into_name_s, 2)?;
  scope.push_root(Value::Object(te_encode_into_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(te_encode_into_fn, Some(intr.function_prototype()))?;

  let encode_into_key = alloc_key(&mut scope, "encodeInto")?;
  scope.define_property(
    te_proto,
    encode_into_key,
    data_desc(Value::Object(te_encode_into_fn)),
  )?;

  // `TextEncoder.prototype.encoding` (read-only accessor property).
  let te_encoding_get_call_id: NativeFunctionId =
    vm.register_native_call(text_encoder_get_encoding)?;
  let te_encoding_get_name_s = scope.alloc_string("get encoding")?;
  scope.push_root(Value::String(te_encoding_get_name_s))?;
  let te_encoding_get_slots = [Value::String(utf8_s)];
  let te_encoding_get_fn = scope.alloc_native_function_with_slots(
    te_encoding_get_call_id,
    None,
    te_encoding_get_name_s,
    0,
    &te_encoding_get_slots,
  )?;
  scope.push_root(Value::Object(te_encoding_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(te_encoding_get_fn, Some(intr.function_prototype()))?;

  let encoding_key = alloc_key(&mut scope, "encoding")?;
  scope.define_property(
    te_proto,
    encoding_key,
    accessor_desc(Value::Object(te_encoding_get_fn), Value::Undefined),
  )?;

  // @@toStringTag
  let te_tag_s = scope.alloc_string("TextEncoder")?;
  scope.push_root(Value::String(te_tag_s))?;
  scope.define_property(
    te_proto,
    PropertyKey::Symbol(intr.well_known_symbols().to_string_tag),
    read_only_data_desc(Value::String(te_tag_s)),
  )?;

  let te_key = alloc_key(&mut scope, "TextEncoder")?;
  scope.define_property(global, te_key, data_desc(Value::Object(te_ctor)))?;

  // --- TextEncoderStream -----------------------------------------------------
  let tes_call_id: NativeFunctionId = vm.register_native_call(text_encoder_stream_call)?;
  let tes_construct_id: NativeConstructId =
    vm.register_native_construct(text_encoder_stream_construct)?;

  let transform_stream_key_s = scope.alloc_string("TransformStream")?;
  scope.push_root(Value::String(transform_stream_key_s))?;
  let readable_key_s = scope.alloc_string("readable")?;
  scope.push_root(Value::String(readable_key_s))?;
  let writable_key_s = scope.alloc_string("writable")?;
  scope.push_root(Value::String(writable_key_s))?;
  let transform_key_s = scope.alloc_string("transform")?;
  scope.push_root(Value::String(transform_key_s))?;
  let enqueue_key_s = scope.alloc_string("enqueue")?;
  scope.push_root(Value::String(enqueue_key_s))?;

  let tes_transform_call_id: NativeFunctionId =
    vm.register_native_call(text_encoder_stream_transform)?;
  let tes_transform_name_s = scope.alloc_string("transform")?;
  scope.push_root(Value::String(tes_transform_name_s))?;
  let tes_transform_slots = [Value::String(enqueue_key_s)];
  let tes_transform_fn = scope.alloc_native_function_with_slots(
    tes_transform_call_id,
    None,
    tes_transform_name_s,
    2,
    &tes_transform_slots,
  )?;
  scope.push_root(Value::Object(tes_transform_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(tes_transform_fn, Some(intr.function_prototype()))?;

  let tes_name_s = scope.alloc_string("TextEncoderStream")?;
  scope.push_root(Value::String(tes_name_s))?;

  let tes_ctor_slots = [
    Value::Object(global),
    Value::String(transform_stream_key_s),
    Value::String(readable_key_s),
    Value::String(writable_key_s),
    Value::String(transform_key_s),
    Value::Object(tes_transform_fn),
  ];
  let tes_ctor = scope.alloc_native_function_with_slots(
    tes_call_id,
    Some(tes_construct_id),
    tes_name_s,
    0,
    &tes_ctor_slots,
  )?;
  scope.push_root(Value::Object(tes_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(tes_ctor, Some(intr.function_prototype()))?;

  let tes_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(tes_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TextEncoderStream constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(tes_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(tes_proto, Some(intr.object_prototype()))?;

  // `TextEncoderStream.prototype.encoding` (read-only accessor property).
  let tes_encoding_get_call_id: NativeFunctionId =
    vm.register_native_call(text_encoder_stream_get_encoding)?;
  let tes_encoding_get_name_s = scope.alloc_string("get encoding")?;
  scope.push_root(Value::String(tes_encoding_get_name_s))?;
  let tes_encoding_get_slots = [Value::String(utf8_s)];
  let tes_encoding_get_fn = scope.alloc_native_function_with_slots(
    tes_encoding_get_call_id,
    None,
    tes_encoding_get_name_s,
    0,
    &tes_encoding_get_slots,
  )?;
  scope.push_root(Value::Object(tes_encoding_get_fn))?;
  scope.heap_mut().object_set_prototype(
    tes_encoding_get_fn,
    Some(intr.function_prototype()),
  )?;

  let encoding_key = alloc_key(&mut scope, "encoding")?;
  scope.define_property(
    tes_proto,
    encoding_key,
    accessor_desc(Value::Object(tes_encoding_get_fn), Value::Undefined),
  )?;

  // @@toStringTag
  let tes_tag_s = scope.alloc_string("TextEncoderStream")?;
  scope.push_root(Value::String(tes_tag_s))?;
  scope.define_property(
    tes_proto,
    PropertyKey::Symbol(intr.well_known_symbols().to_string_tag),
    read_only_data_desc(Value::String(tes_tag_s)),
  )?;

  let tes_key = alloc_key(&mut scope, "TextEncoderStream")?;
  scope.define_property(global, tes_key, data_desc(Value::Object(tes_ctor)))?;

  // --- TextDecoderStream -----------------------------------------------------
  let tds_call_id: NativeFunctionId = vm.register_native_call(text_decoder_stream_call)?;
  let tds_construct_id: NativeConstructId =
    vm.register_native_construct(text_decoder_stream_construct)?;

  // Reuse common TransformStream keys from the TextEncoderStream setup above.
  let flush_key_s = scope.alloc_string("flush")?;
  scope.push_root(Value::String(flush_key_s))?;

  let tds_transform_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_stream_transform)?;
  let tds_transform_name_s = scope.alloc_string("transform")?;
  scope.push_root(Value::String(tds_transform_name_s))?;
  let tds_transform_slots = [Value::String(enqueue_key_s)];
  let tds_transform_fn = scope.alloc_native_function_with_slots(
    tds_transform_call_id,
    None,
    tds_transform_name_s,
    2,
    &tds_transform_slots,
  )?;
  scope.push_root(Value::Object(tds_transform_fn))?;
  scope.heap_mut().object_set_prototype(
    tds_transform_fn,
    Some(intr.function_prototype()),
  )?;

  let tds_flush_call_id: NativeFunctionId = vm.register_native_call(text_decoder_stream_flush)?;
  let tds_flush_name_s = scope.alloc_string("flush")?;
  scope.push_root(Value::String(tds_flush_name_s))?;
  let tds_flush_slots = [Value::String(enqueue_key_s)];
  let tds_flush_fn = scope.alloc_native_function_with_slots(
    tds_flush_call_id,
    None,
    tds_flush_name_s,
    1,
    &tds_flush_slots,
  )?;
  scope.push_root(Value::Object(tds_flush_fn))?;
  scope.heap_mut().object_set_prototype(tds_flush_fn, Some(intr.function_prototype()))?;

  let tds_name_s = scope.alloc_string("TextDecoderStream")?;
  scope.push_root(Value::String(tds_name_s))?;

  let tds_ctor_slots = [
    Value::Object(global),
    Value::String(transform_stream_key_s),
    Value::String(readable_key_s),
    Value::String(writable_key_s),
    Value::String(transform_key_s),
    Value::String(flush_key_s),
    Value::Object(tds_transform_fn),
    Value::Object(tds_flush_fn),
  ];
  let tds_ctor = scope.alloc_native_function_with_slots(
    tds_call_id,
    Some(tds_construct_id),
    tds_name_s,
    0,
    &tds_ctor_slots,
  )?;
  scope.push_root(Value::Object(tds_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(tds_ctor, Some(intr.function_prototype()))?;

  let tds_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(tds_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TextDecoderStream constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(tds_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(tds_proto, Some(intr.object_prototype()))?;

  // `TextDecoderStream.prototype.encoding` / `fatal` / `ignoreBOM` (read-only accessor properties).
  let tds_encoding_get_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_stream_get_encoding)?;
  let tds_encoding_get_name_s = scope.alloc_string("get encoding")?;
  scope.push_root(Value::String(tds_encoding_get_name_s))?;
  let tds_encoding_get_fn =
    scope.alloc_native_function(tds_encoding_get_call_id, None, tds_encoding_get_name_s, 0)?;
  scope.push_root(Value::Object(tds_encoding_get_fn))?;
  scope.heap_mut().object_set_prototype(
    tds_encoding_get_fn,
    Some(intr.function_prototype()),
  )?;
  let encoding_key = alloc_key(&mut scope, "encoding")?;
  scope.define_property(
    tds_proto,
    encoding_key,
    accessor_desc(Value::Object(tds_encoding_get_fn), Value::Undefined),
  )?;

  let tds_fatal_get_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_stream_get_fatal)?;
  let tds_fatal_get_name_s = scope.alloc_string("get fatal")?;
  scope.push_root(Value::String(tds_fatal_get_name_s))?;
  let tds_fatal_get_fn =
    scope.alloc_native_function(tds_fatal_get_call_id, None, tds_fatal_get_name_s, 0)?;
  scope.push_root(Value::Object(tds_fatal_get_fn))?;
  scope.heap_mut().object_set_prototype(
    tds_fatal_get_fn,
    Some(intr.function_prototype()),
  )?;
  let fatal_key = alloc_key(&mut scope, "fatal")?;
  scope.define_property(
    tds_proto,
    fatal_key,
    accessor_desc(Value::Object(tds_fatal_get_fn), Value::Undefined),
  )?;

  let tds_ignore_bom_get_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_stream_get_ignore_bom)?;
  let tds_ignore_bom_get_name_s = scope.alloc_string("get ignoreBOM")?;
  scope.push_root(Value::String(tds_ignore_bom_get_name_s))?;
  let tds_ignore_bom_get_fn = scope.alloc_native_function(
    tds_ignore_bom_get_call_id,
    None,
    tds_ignore_bom_get_name_s,
    0,
  )?;
  scope.push_root(Value::Object(tds_ignore_bom_get_fn))?;
  scope.heap_mut().object_set_prototype(
    tds_ignore_bom_get_fn,
    Some(intr.function_prototype()),
  )?;
  let ignore_bom_key = alloc_key(&mut scope, "ignoreBOM")?;
  scope.define_property(
    tds_proto,
    ignore_bom_key,
    accessor_desc(Value::Object(tds_ignore_bom_get_fn), Value::Undefined),
  )?;

  // @@toStringTag
  let tds_tag_s = scope.alloc_string("TextDecoderStream")?;
  scope.push_root(Value::String(tds_tag_s))?;
  scope.define_property(
    tds_proto,
    PropertyKey::Symbol(intr.well_known_symbols().to_string_tag),
    read_only_data_desc(Value::String(tds_tag_s)),
  )?;

  let tds_key = alloc_key(&mut scope, "TextDecoderStream")?;
  scope.define_property(global, tds_key, data_desc(Value::Object(tds_ctor)))?;

  // --- TextDecoder -----------------------------------------------------------
  let td_call_id: NativeFunctionId = vm.register_native_call(text_decoder_call)?;
  let td_construct_id: NativeConstructId = vm.register_native_construct(text_decoder_construct)?;
  let td_name_s = scope.alloc_string("TextDecoder")?;
  scope.push_root(Value::String(td_name_s))?;
  let td_ctor = scope.alloc_native_function(td_call_id, Some(td_construct_id), td_name_s, 0)?;
  scope.push_root(Value::Object(td_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(td_ctor, Some(intr.function_prototype()))?;

  let td_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(td_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "TextDecoder constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(td_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(td_proto, Some(intr.object_prototype()))?;

  let td_decode_call_id: NativeFunctionId = vm.register_native_call(text_decoder_decode)?;
  let td_decode_name_s = scope.alloc_string("decode")?;
  scope.push_root(Value::String(td_decode_name_s))?;
  let td_decode_fn = scope.alloc_native_function(td_decode_call_id, None, td_decode_name_s, 1)?;
  scope.push_root(Value::Object(td_decode_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(td_decode_fn, Some(intr.function_prototype()))?;

  let decode_key = alloc_key(&mut scope, "decode")?;
  scope.define_property(td_proto, decode_key, data_desc(Value::Object(td_decode_fn)))?;

  // `TextDecoder.prototype.encoding` / `fatal` / `ignoreBOM` (read-only accessor properties).
  let td_encoding_get_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_get_encoding)?;
  let td_encoding_get_name_s = scope.alloc_string("get encoding")?;
  scope.push_root(Value::String(td_encoding_get_name_s))?;
  let td_encoding_get_fn =
    scope.alloc_native_function(td_encoding_get_call_id, None, td_encoding_get_name_s, 0)?;
  scope.push_root(Value::Object(td_encoding_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(td_encoding_get_fn, Some(intr.function_prototype()))?;
  let encoding_key = alloc_key(&mut scope, "encoding")?;
  scope.define_property(
    td_proto,
    encoding_key,
    accessor_desc(Value::Object(td_encoding_get_fn), Value::Undefined),
  )?;

  let td_fatal_get_call_id: NativeFunctionId = vm.register_native_call(text_decoder_get_fatal)?;
  let td_fatal_get_name_s = scope.alloc_string("get fatal")?;
  scope.push_root(Value::String(td_fatal_get_name_s))?;
  let td_fatal_get_fn =
    scope.alloc_native_function(td_fatal_get_call_id, None, td_fatal_get_name_s, 0)?;
  scope.push_root(Value::Object(td_fatal_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(td_fatal_get_fn, Some(intr.function_prototype()))?;
  let fatal_key = alloc_key(&mut scope, "fatal")?;
  scope.define_property(
    td_proto,
    fatal_key,
    accessor_desc(Value::Object(td_fatal_get_fn), Value::Undefined),
  )?;

  let td_ignore_bom_get_call_id: NativeFunctionId =
    vm.register_native_call(text_decoder_get_ignore_bom)?;
  let td_ignore_bom_get_name_s = scope.alloc_string("get ignoreBOM")?;
  scope.push_root(Value::String(td_ignore_bom_get_name_s))?;
  let td_ignore_bom_get_fn =
    scope.alloc_native_function(td_ignore_bom_get_call_id, None, td_ignore_bom_get_name_s, 0)?;
  scope.push_root(Value::Object(td_ignore_bom_get_fn))?;
  scope
    .heap_mut()
    .object_set_prototype(td_ignore_bom_get_fn, Some(intr.function_prototype()))?;
  let ignore_bom_key = alloc_key(&mut scope, "ignoreBOM")?;
  scope.define_property(
    td_proto,
    ignore_bom_key,
    accessor_desc(Value::Object(td_ignore_bom_get_fn), Value::Undefined),
  )?;

  // @@toStringTag
  let td_tag_s = scope.alloc_string("TextDecoder")?;
  scope.push_root(Value::String(td_tag_s))?;
  scope.define_property(
    td_proto,
    PropertyKey::Symbol(intr.well_known_symbols().to_string_tag),
    read_only_data_desc(Value::String(td_tag_s)),
  )?;

  let td_key = alloc_key(&mut scope, "TextDecoder")?;
  scope.define_property(global, td_key, data_desc(Value::Object(td_ctor)))?;

  Ok(())
  })();

  if let Err(err) = result {
    if let Ok(mut map) = text_encoding_contexts().lock() {
      map.remove(&heap_key);
    }
    return Err(err);
  }

  Ok(TextEncodingBindings { heap_key })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_streams::install_window_streams_bindings;
  use vm_js::{Heap, HeapLimits, JsRuntime, VmOptions};

  /// Fallible `Box::new` that returns `VmError::OutOfMemory` instead of aborting the process.
  #[inline]
  fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
    // `Box::new` does not allocate for ZSTs, so it cannot fail with OOM.
    if std::mem::size_of::<T>() == 0 {
      return Ok(Box::new(value));
    }

    let layout = std::alloc::Layout::new::<T>();
    // SAFETY: `alloc` returns either a suitably aligned block of memory for `T` or null on OOM. We
    // write `value` into it and transfer ownership to `Box`.
    unsafe {
      let ptr = std::alloc::alloc(layout) as *mut T;
      if ptr.is_null() {
        return Err(VmError::OutOfMemory);
      }
      ptr.write(value);
      Ok(Box::from_raw(ptr))
    }
  }

  fn get_string(heap: &vm_js::Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn new_runtime_with_streams_and_text_encoding(
  ) -> Result<(Box<JsRuntime>, TextEncodingBindings), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let rt = JsRuntime::new(vm, heap)?;
    let mut rt = box_try_new_vm(rt)?;

    let bindings = {
      let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
      install_window_streams_bindings(vm, realm, heap)?;
      install_window_text_encoding_bindings(vm, realm, heap)?
    };

    Ok((rt, bindings))
  }

  #[test]
  fn text_encoder_utf8_encodes_strings() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    let encoder_ctor_key = alloc_key(&mut scope, "TextEncoder")?;
    let ctor = vm.get(&mut scope, global, encoder_ctor_key)?;

    // new TextEncoder().encode("hi")
    let Value::Object(_ctor_obj) = ctor else {
      return Err(VmError::InvariantViolation("TextEncoder missing"));
    };
    let enc = vm.construct_without_host(&mut scope, ctor, &[], ctor)?;
    let Value::Object(enc_obj) = enc else {
      return Err(VmError::InvariantViolation(
        "TextEncoder construct must return object",
      ));
    };
    scope.push_root(Value::Object(enc_obj))?;

    let encode_key = alloc_key(&mut scope, "encode")?;
    let encode_fn = vm.get(&mut scope, enc_obj, encode_key)?;
    let hi_s = scope.alloc_string("hi")?;
    scope.push_root(Value::String(hi_s))?;
    let out = vm.call_without_host(
      &mut scope,
      encode_fn,
      Value::Object(enc_obj),
      &[Value::String(hi_s)],
    )?;

    let Value::Object(out_obj) = out else {
      return Err(VmError::InvariantViolation("encode must return object"));
    };
    assert!(
      scope.heap().is_uint8_array_object(out_obj),
      "expected encode() to return a Uint8Array"
    );

    // Read the first two bytes via `.buffer` + `.byteOffset`.
    let data = scope.heap().uint8_array_data(out_obj)?;
    assert_eq!(data, b"hi");

    // `encoding` should be "utf-8".
    let encoding_key = alloc_key(&mut scope, "encoding")?;
    let encoding_val = vm.get(&mut scope, enc_obj, encoding_key)?;
    assert_eq!(get_string(scope.heap(), encoding_val), "utf-8");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_encoder_encode_coerces_string_object() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    let out = rt.exec_script("new TextEncoder().encode(new String('hi'))")?;
    let Value::Object(out_obj) = out else {
      return Err(VmError::InvariantViolation(
        "TextEncoder.encode must return an object",
      ));
    };
    assert!(
      rt.heap().is_uint8_array_object(out_obj),
      "expected Uint8Array result"
    );
    assert_eq!(rt.heap().uint8_array_data(out_obj)?, b"hi");
    Ok(())
  }

  #[test]
  fn text_encoder_encode_coerces_object_via_to_string() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    let out = rt.exec_script("new TextEncoder().encode({ toString(){ return 'ok'; } })")?;
    let Value::Object(out_obj) = out else {
      return Err(VmError::InvariantViolation(
        "TextEncoder.encode must return an object",
      ));
    };
    assert!(
      rt.heap().is_uint8_array_object(out_obj),
      "expected Uint8Array result"
    );
    assert_eq!(rt.heap().uint8_array_data(out_obj)?, b"ok");
    Ok(())
  }

  #[test]
  fn text_decoder_coerces_string_object_label() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    let decoded = rt.exec_script(
      "new TextDecoder(new String('windows-1252')).decode(new Uint8Array([0x80]))",
    )?;
    assert_eq!(get_string(rt.heap(), decoded), "€");
    Ok(())
  }

  #[test]
  fn text_decoder_utf8_decodes_uint8_array() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let decoder = vm.construct_without_host(&mut scope, decoder_ctor, &[], decoder_ctor)?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation(
        "TextDecoder must construct object",
      ));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    // Build a Uint8Array containing "ok".
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let u8 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(2.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(u8_obj) = u8 else {
      return Err(VmError::InvariantViolation(
        "Uint8Array construct must return object",
      ));
    };
    scope.push_root(Value::Object(u8_obj))?;

    // u8[0] = 111; u8[1] = 107;
    let key0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(key0_s))?;
    let key0 = PropertyKey::from_string(key0_s);
    scope.ordinary_set(
      &mut vm,
      u8_obj,
      key0,
      Value::Number(111.0),
      Value::Object(u8_obj),
    )?;
    let key1_s = scope.alloc_string("1")?;
    scope.push_root(Value::String(key1_s))?;
    let key1 = PropertyKey::from_string(key1_s);
    scope.ordinary_set(
      &mut vm,
      u8_obj,
      key1,
      Value::Number(107.0),
      Value::Object(u8_obj),
    )?;

    let decode_key = alloc_key(&mut scope, "decode")?;
    let decode_fn = vm.get(&mut scope, decoder_obj, decode_key)?;
    let decoded = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(u8_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded), "ok");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_utf8_decodes_data_view_slice() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextDecoder()
    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let decoder = vm.construct_without_host(&mut scope, decoder_ctor, &[], decoder_ctor)?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation(
        "TextDecoder must construct object",
      ));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    // Create a backing buffer containing: "a€bc" (as UTF-8 bytes).
    // We'll decode only the "€" bytes via a DataView slice.
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let u8 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(6.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(u8_obj) = u8 else {
      return Err(VmError::InvariantViolation(
        "Uint8Array construct must return object",
      ));
    };
    scope.push_root(Value::Object(u8_obj))?;

    let set_index = |vm: &mut Vm, scope: &mut Scope<'_>, arr: vm_js::GcObject, idx: usize, val: u8| {
      let key_s = scope.alloc_string(&idx.to_string())?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      scope.ordinary_set(
        vm,
        arr,
        key,
        Value::Number(val as f64),
        Value::Object(arr),
      )?;
      Ok::<(), VmError>(())
    };

    // "a€bc" -> [0x61, 0xE2, 0x82, 0xAC, 0x62, 0x63]
    set_index(&mut vm, &mut scope, u8_obj, 0, 0x61)?;
    set_index(&mut vm, &mut scope, u8_obj, 1, 0xE2)?;
    set_index(&mut vm, &mut scope, u8_obj, 2, 0x82)?;
    set_index(&mut vm, &mut scope, u8_obj, 3, 0xAC)?;
    set_index(&mut vm, &mut scope, u8_obj, 4, 0x62)?;
    set_index(&mut vm, &mut scope, u8_obj, 5, 0x63)?;

    let buffer_key = alloc_key(&mut scope, "buffer")?;
    let buffer_val = vm.get(&mut scope, u8_obj, buffer_key)?;
    let Value::Object(buffer_obj) = buffer_val else {
      return Err(VmError::InvariantViolation(
        "Uint8Array.buffer must return an object",
      ));
    };
    scope.push_root(Value::Object(buffer_obj))?;

    let data_view_ctor_key = alloc_key(&mut scope, "DataView")?;
    let data_view_ctor = vm.get(&mut scope, global, data_view_ctor_key)?;
    let Value::Object(data_view_ctor_obj) = data_view_ctor else {
      return Err(VmError::InvariantViolation("DataView missing"));
    };

    // new DataView(buffer, 1, 3) -> view over the UTF-8 bytes for "€"
    let view = vm.construct_without_host(
      &mut scope,
      data_view_ctor,
      &[Value::Object(buffer_obj), Value::Number(1.0), Value::Number(3.0)],
      Value::Object(data_view_ctor_obj),
    )?;
    let Value::Object(view_obj) = view else {
      return Err(VmError::InvariantViolation(
        "DataView construct must return object",
      ));
    };
    scope.push_root(Value::Object(view_obj))?;

    let decode_key = alloc_key(&mut scope, "decode")?;
    let decode_fn = vm.get(&mut scope, decoder_obj, decode_key)?;
    let decoded = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(view_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded), "€");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_decode_streaming_preserves_partial_utf8_sequences() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let intr = realm.intrinsics();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextDecoder()
    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let decoder = vm.construct_without_host(&mut scope, decoder_ctor, &[], decoder_ctor)?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation(
        "TextDecoder must construct object",
      ));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    // Options object: `{ stream: true }`.
    let options_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(options_obj))?;
    scope
      .heap_mut()
      .object_set_prototype(options_obj, Some(intr.object_prototype()))?;
    let stream_key = alloc_key(&mut scope, "stream")?;
    scope.define_property(options_obj, stream_key, data_desc(Value::Bool(true)))?;

    // Shared helpers: Uint8Array constructor and decoder.decode function.
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let decode_key = alloc_key(&mut scope, "decode")?;
    let decode_fn = vm.get(&mut scope, decoder_obj, decode_key)?;

    // chunk1 = new Uint8Array([0xE2, 0x82]) (partial "€" sequence).
    let chunk1 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(2.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(chunk1_obj) = chunk1 else {
      return Err(VmError::InvariantViolation(
        "Uint8Array construct must return object",
      ));
    };
    scope.push_root(Value::Object(chunk1_obj))?;
    let key0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(key0_s))?;
    let key0 = PropertyKey::from_string(key0_s);
    scope.ordinary_set(
      &mut vm,
      chunk1_obj,
      key0,
      Value::Number(0xE2 as f64),
      Value::Object(chunk1_obj),
    )?;
    let key1_s = scope.alloc_string("1")?;
    scope.push_root(Value::String(key1_s))?;
    let key1 = PropertyKey::from_string(key1_s);
    scope.ordinary_set(
      &mut vm,
      chunk1_obj,
      key1,
      Value::Number(0x82 as f64),
      Value::Object(chunk1_obj),
    )?;

    let decoded1 = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(chunk1_obj), Value::Object(options_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded1), "");

    // chunk2 = new Uint8Array([0xAC]) (completes "€").
    let chunk2 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(1.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(chunk2_obj) = chunk2 else {
      return Err(VmError::InvariantViolation(
        "Uint8Array construct must return object",
      ));
    };
    scope.push_root(Value::Object(chunk2_obj))?;
    let key0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(key0_s))?;
    let key0 = PropertyKey::from_string(key0_s);
    scope.ordinary_set(
      &mut vm,
      chunk2_obj,
      key0,
      Value::Number(0xAC as f64),
      Value::Object(chunk2_obj),
    )?;

    let decoded2 = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(chunk2_obj), Value::Object(options_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded2), "€");

    // Flush/reset: decoder.decode() with stream=false default should clear any buffered state.
    let flushed = vm.call_without_host(&mut scope, decode_fn, Value::Object(decoder_obj), &[])?;
    assert_eq!(get_string(scope.heap(), flushed), "");

    // After reset, decoding a continuation byte alone should produce U+FFFD.
    let decoded_after_reset = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(chunk2_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded_after_reset), "�");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_windows_1252_decodes_euro_sign() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextDecoder('windows-1252')
    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let Value::Object(decoder_ctor_obj) = decoder_ctor else {
      return Err(VmError::InvariantViolation("TextDecoder missing"));
    };
    let label = scope.alloc_string("windows-1252")?;
    scope.push_root(Value::String(label))?;
    let decoder = vm.construct_without_host(
      &mut scope,
      decoder_ctor,
      &[Value::String(label)],
      Value::Object(decoder_ctor_obj),
    )?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation("TextDecoder must construct object"));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    // Ensure `.encoding` returns canonical lowercase label.
    let encoding_key = alloc_key(&mut scope, "encoding")?;
    let encoding_val = vm.get(&mut scope, decoder_obj, encoding_key)?;
    assert_eq!(get_string(scope.heap(), encoding_val), "windows-1252");

    // new Uint8Array([0x80])
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let u8 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(1.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(u8_obj) = u8 else {
      return Err(VmError::InvariantViolation("Uint8Array construct must return object"));
    };
    scope.push_root(Value::Object(u8_obj))?;

    let key0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(key0_s))?;
    let key0 = PropertyKey::from_string(key0_s);
    scope.ordinary_set(&mut vm, u8_obj, key0, Value::Number(0x80 as f64), Value::Object(u8_obj))?;

    // decoder.decode(u8) === "€"
    let decode_key = alloc_key(&mut scope, "decode")?;
    let decode_fn = vm.get(&mut scope, decoder_obj, decode_key)?;
    let decoded = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(u8_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded), "€");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_utf16le_decodes_basic_code_unit() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextDecoder('utf-16le')
    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let Value::Object(decoder_ctor_obj) = decoder_ctor else {
      return Err(VmError::InvariantViolation("TextDecoder missing"));
    };
    let label = scope.alloc_string("utf-16le")?;
    scope.push_root(Value::String(label))?;
    let decoder = vm.construct_without_host(
      &mut scope,
      decoder_ctor,
      &[Value::String(label)],
      Value::Object(decoder_ctor_obj),
    )?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation("TextDecoder must construct object"));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    let encoding_key = alloc_key(&mut scope, "encoding")?;
    let encoding_val = vm.get(&mut scope, decoder_obj, encoding_key)?;
    assert_eq!(get_string(scope.heap(), encoding_val), "utf-16le");

    // Uint8Array.of(0x61, 0x00) => "a"
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let u8 = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(2.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(u8_obj) = u8 else {
      return Err(VmError::InvariantViolation("Uint8Array construct must return object"));
    };
    scope.push_root(Value::Object(u8_obj))?;

    let key0_s = scope.alloc_string("0")?;
    scope.push_root(Value::String(key0_s))?;
    let key0 = PropertyKey::from_string(key0_s);
    scope.ordinary_set(&mut vm, u8_obj, key0, Value::Number(0x61 as f64), Value::Object(u8_obj))?;

    let key1_s = scope.alloc_string("1")?;
    scope.push_root(Value::String(key1_s))?;
    let key1 = PropertyKey::from_string(key1_s);
    scope.ordinary_set(&mut vm, u8_obj, key1, Value::Number(0.0), Value::Object(u8_obj))?;

    let decode_key = alloc_key(&mut scope, "decode")?;
    let decode_fn = vm.get(&mut scope, decoder_obj, decode_key)?;
    let decoded = vm.call_without_host(
      &mut scope,
      decode_fn,
      Value::Object(decoder_obj),
      &[Value::Object(u8_obj)],
    )?;
    assert_eq!(get_string(scope.heap(), decoded), "a");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn object_prototype_to_string_uses_text_encoding_to_string_tags() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let intr = realm.intrinsics();
    let obj_proto = intr.object_prototype();

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    scope.push_root(Value::Object(obj_proto))?;

    let to_string_key = alloc_key(&mut scope, "toString")?;
    let to_string_fn = vm.get(&mut scope, obj_proto, to_string_key)?;

    let te_ctor_key = alloc_key(&mut scope, "TextEncoder")?;
    let te_ctor = vm.get(&mut scope, global, te_ctor_key)?;
    let Value::Object(te_ctor_obj) = te_ctor else {
      return Err(VmError::InvariantViolation("TextEncoder missing"));
    };
    let te = vm.construct_without_host(&mut scope, te_ctor, &[], Value::Object(te_ctor_obj))?;
    let Value::Object(te_obj) = te else {
      return Err(VmError::InvariantViolation(
        "TextEncoder constructor must return object",
      ));
    };
    let te_tag = vm.call_without_host(&mut scope, to_string_fn, Value::Object(te_obj), &[])?;
    assert_eq!(get_string(scope.heap(), te_tag), "[object TextEncoder]");

    let td_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let td_ctor = vm.get(&mut scope, global, td_ctor_key)?;
    let Value::Object(td_ctor_obj) = td_ctor else {
      return Err(VmError::InvariantViolation("TextDecoder missing"));
    };
    let td = vm.construct_without_host(&mut scope, td_ctor, &[], Value::Object(td_ctor_obj))?;
    let Value::Object(td_obj) = td else {
      return Err(VmError::InvariantViolation(
        "TextDecoder constructor must return object",
      ));
    };
    let td_tag = vm.call_without_host(&mut scope, to_string_fn, Value::Object(td_obj), &[])?;
    assert_eq!(get_string(scope.heap(), td_tag), "[object TextDecoder]");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_encoder_encode_into_writes_into_destination_and_returns_counts() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextEncoder()
    let encoder_ctor_key = alloc_key(&mut scope, "TextEncoder")?;
    let ctor = vm.get(&mut scope, global, encoder_ctor_key)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::InvariantViolation("TextEncoder missing"));
    };
    let enc = vm.construct_without_host(&mut scope, ctor, &[], Value::Object(ctor_obj))?;
    let Value::Object(enc_obj) = enc else {
      return Err(VmError::InvariantViolation(
        "TextEncoder construct must return object",
      ));
    };
    scope.push_root(Value::Object(enc_obj))?;

    // new Uint8Array(1)
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let dest = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(1.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(dest_obj) = dest else {
      return Err(VmError::InvariantViolation(
        "Uint8Array must construct object",
      ));
    };
    scope.push_root(Value::Object(dest_obj))?;

    // enc.encodeInto("hi", dest)
    let encode_into_key = alloc_key(&mut scope, "encodeInto")?;
    let encode_into_fn = vm.get(&mut scope, enc_obj, encode_into_key)?;
    let hi_s = scope.alloc_string("hi")?;
    scope.push_root(Value::String(hi_s))?;
    let out = vm.call_without_host(
      &mut scope,
      encode_into_fn,
      Value::Object(enc_obj),
      &[Value::String(hi_s), Value::Object(dest_obj)],
    )?;

    // Destination should contain only "h" (dest is too small for "hi").
    let data = scope.heap().uint8_array_data(dest_obj)?;
    assert_eq!(data, b"h");

    // Result should be `{ read: 1, written: 1 }`.
    let Value::Object(out_obj) = out else {
      return Err(VmError::InvariantViolation("encodeInto must return object"));
    };
    scope.push_root(Value::Object(out_obj))?;
    let read_key = alloc_key(&mut scope, "read")?;
    let written_key = alloc_key(&mut scope, "written")?;
    let read_val = vm.get(&mut scope, out_obj, read_key)?;
    let written_val = vm.get(&mut scope, out_obj, written_key)?;
    assert_eq!(read_val, Value::Number(1.0));
    assert_eq!(written_val, Value::Number(1.0));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_encoder_encode_into_does_not_write_partial_multi_byte_sequences() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // new TextEncoder()
    let encoder_ctor_key = alloc_key(&mut scope, "TextEncoder")?;
    let ctor = vm.get(&mut scope, global, encoder_ctor_key)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::InvariantViolation("TextEncoder missing"));
    };
    let enc = vm.construct_without_host(&mut scope, ctor, &[], Value::Object(ctor_obj))?;
    let Value::Object(enc_obj) = enc else {
      return Err(VmError::InvariantViolation(
        "TextEncoder construct must return object",
      ));
    };
    scope.push_root(Value::Object(enc_obj))?;

    // new Uint8Array(2)
    let u8_ctor_key = alloc_key(&mut scope, "Uint8Array")?;
    let u8_ctor = vm.get(&mut scope, global, u8_ctor_key)?;
    let Value::Object(u8_ctor_obj) = u8_ctor else {
      return Err(VmError::InvariantViolation("Uint8Array missing"));
    };
    let dest = vm.construct_without_host(
      &mut scope,
      u8_ctor,
      &[Value::Number(2.0)],
      Value::Object(u8_ctor_obj),
    )?;
    let Value::Object(dest_obj) = dest else {
      return Err(VmError::InvariantViolation(
        "Uint8Array must construct object",
      ));
    };
    scope.push_root(Value::Object(dest_obj))?;

    // enc.encodeInto("€", dest) where "€" is 3-byte UTF-8.
    let encode_into_key = alloc_key(&mut scope, "encodeInto")?;
    let encode_into_fn = vm.get(&mut scope, enc_obj, encode_into_key)?;
    let euro_s = scope.alloc_string("€")?;
    scope.push_root(Value::String(euro_s))?;
    let out = vm.call_without_host(
      &mut scope,
      encode_into_fn,
      Value::Object(enc_obj),
      &[Value::String(euro_s), Value::Object(dest_obj)],
    )?;

    let data = scope.heap().uint8_array_data(dest_obj)?;
    assert_eq!(data, &[0, 0], "expected destination to remain unchanged");

    let Value::Object(out_obj) = out else {
      return Err(VmError::InvariantViolation("encodeInto must return object"));
    };
    scope.push_root(Value::Object(out_obj))?;
    let read_key = alloc_key(&mut scope, "read")?;
    let written_key = alloc_key(&mut scope, "written")?;
    let read_val = vm.get(&mut scope, out_obj, read_key)?;
    let written_val = vm.get(&mut scope, out_obj, written_key)?;
    assert_eq!(read_val, Value::Number(0.0));
    assert_eq!(written_val, Value::Number(0.0));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_rejects_invalid_encoding_label() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let Value::Object(decoder_ctor_obj) = decoder_ctor else {
      return Err(VmError::InvariantViolation("TextDecoder missing"));
    };

    let bad_label = scope.alloc_string("definitely-not-an-encoding")?;
    scope.push_root(Value::String(bad_label))?;
    let err = vm
      .construct_without_host(
        &mut scope,
        decoder_ctor,
        &[Value::String(bad_label)],
        Value::Object(decoder_ctor_obj),
      )
      .expect_err("expected invalid label to throw");
    let (VmError::Throw(value) | VmError::ThrowWithStack { value, .. }) = err else {
      return Err(VmError::InvariantViolation("expected a thrown exception"));
    };
    let Value::Object(obj) = value else {
      return Err(VmError::InvariantViolation("expected error object"));
    };
    scope.push_root(Value::Object(obj))?;
    let name_key = alloc_key(&mut scope, "name")?;
    let name = vm.get(&mut scope, obj, name_key)?;
    assert_eq!(get_string(scope.heap(), name), "RangeError");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_attributes_live_on_prototype_and_to_string_tag_is_set() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _bindings = install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let intr = realm.intrinsics();
    let to_string_tag_key = PropertyKey::Symbol(intr.well_known_symbols().to_string_tag);

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // --- TextDecoder prototype accessors -------------------------------------
    let td_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let td_ctor = vm.get(&mut scope, global, td_ctor_key)?;
    let Value::Object(td_ctor_obj) = td_ctor else {
      return Err(VmError::InvariantViolation(
        "TextDecoder constructor missing",
      ));
    };
    scope.push_root(Value::Object(td_ctor_obj))?;

    let proto_key = alloc_key(&mut scope, "prototype")?;
    let td_proto_val = vm.get(&mut scope, td_ctor_obj, proto_key)?;
    let Value::Object(td_proto_obj) = td_proto_val else {
      return Err(VmError::InvariantViolation("TextDecoder.prototype missing"));
    };
    scope.push_root(Value::Object(td_proto_obj))?;

    let encoding_key = alloc_key(&mut scope, "encoding")?;
    let fatal_key = alloc_key(&mut scope, "fatal")?;
    let ignore_bom_key = alloc_key(&mut scope, "ignoreBOM")?;

    let enc_desc = scope
      .heap()
      .object_get_own_property(td_proto_obj, &encoding_key)?
      .ok_or(VmError::InvariantViolation(
        "TextDecoder.prototype.encoding missing",
      ))?;
    assert!(
      matches!(
        enc_desc.kind,
        PropertyKind::Accessor {
          set: Value::Undefined,
          ..
        }
      ),
      "expected encoding to be a read-only accessor property"
    );

    let fatal_desc = scope
      .heap()
      .object_get_own_property(td_proto_obj, &fatal_key)?
      .ok_or(VmError::InvariantViolation(
        "TextDecoder.prototype.fatal missing",
      ))?;
    assert!(
      matches!(
        fatal_desc.kind,
        PropertyKind::Accessor {
          set: Value::Undefined,
          ..
        }
      ),
      "expected fatal to be a read-only accessor property"
    );

    let ignore_desc = scope
      .heap()
      .object_get_own_property(td_proto_obj, &ignore_bom_key)?
      .ok_or(VmError::InvariantViolation(
        "TextDecoder.prototype.ignoreBOM missing",
      ))?;
    assert!(
      matches!(
        ignore_desc.kind,
        PropertyKind::Accessor {
          set: Value::Undefined,
          ..
        }
      ),
      "expected ignoreBOM to be a read-only accessor property"
    );

    let tag_desc = scope
      .heap()
      .object_get_own_property(td_proto_obj, &to_string_tag_key)?
      .ok_or(VmError::InvariantViolation(
        "TextDecoder.prototype @@toStringTag missing",
      ))?;
    let PropertyKind::Data {
      value: Value::String(td_tag),
      writable: false,
    } = tag_desc.kind
    else {
      return Err(VmError::InvariantViolation(
        "TextDecoder @@toStringTag must be a non-writable data property",
      ));
    };
    assert_eq!(
      get_string(scope.heap(), Value::String(td_tag)),
      "TextDecoder"
    );

    // Construct a decoder with `{ fatal: true, ignoreBOM: true }` and ensure it does not get own
    // data properties for those attributes (they should be inherited accessors).
    let options = scope.alloc_object()?;
    scope.push_root(Value::Object(options))?;
    scope
      .heap_mut()
      .object_set_prototype(options, Some(intr.object_prototype()))?;
    scope.define_property(options, fatal_key, data_desc(Value::Bool(true)))?;
    scope.define_property(options, ignore_bom_key, data_desc(Value::Bool(true)))?;

    let label = scope.alloc_string("utf-8")?;
    scope.push_root(Value::String(label))?;
    let decoder = vm.construct_without_host(
      &mut scope,
      td_ctor,
      &[Value::String(label), Value::Object(options)],
      Value::Object(td_ctor_obj),
    )?;
    let Value::Object(decoder_obj) = decoder else {
      return Err(VmError::InvariantViolation(
        "TextDecoder constructor must return object",
      ));
    };
    scope.push_root(Value::Object(decoder_obj))?;

    assert!(
      scope
        .heap()
        .object_get_own_property(decoder_obj, &encoding_key)?
        .is_none(),
      "TextDecoder instances should not have own encoding property"
    );
    assert!(
      scope
        .heap()
        .object_get_own_property(decoder_obj, &fatal_key)?
        .is_none(),
      "TextDecoder instances should not have own fatal property"
    );
    assert!(
      scope
        .heap()
        .object_get_own_property(decoder_obj, &ignore_bom_key)?
        .is_none(),
      "TextDecoder instances should not have own ignoreBOM property"
    );

    let encoding_val = vm.get(&mut scope, decoder_obj, encoding_key)?;
    assert_eq!(get_string(scope.heap(), encoding_val), "utf-8");
    assert_eq!(
      vm.get(&mut scope, decoder_obj, fatal_key)?,
      Value::Bool(true)
    );
    assert_eq!(
      vm.get(&mut scope, decoder_obj, ignore_bom_key)?,
      Value::Bool(true)
    );

    // --- TextEncoder prototype accessors -------------------------------------
    let te_ctor_key = alloc_key(&mut scope, "TextEncoder")?;
    let te_ctor = vm.get(&mut scope, global, te_ctor_key)?;
    let Value::Object(te_ctor_obj) = te_ctor else {
      return Err(VmError::InvariantViolation(
        "TextEncoder constructor missing",
      ));
    };
    scope.push_root(Value::Object(te_ctor_obj))?;

    let te_proto_val = vm.get(&mut scope, te_ctor_obj, proto_key)?;
    let Value::Object(te_proto_obj) = te_proto_val else {
      return Err(VmError::InvariantViolation("TextEncoder.prototype missing"));
    };
    scope.push_root(Value::Object(te_proto_obj))?;

    let te_enc_desc = scope
      .heap()
      .object_get_own_property(te_proto_obj, &encoding_key)?
      .ok_or(VmError::InvariantViolation(
        "TextEncoder.prototype.encoding missing",
      ))?;
    assert!(
      matches!(
        te_enc_desc.kind,
        PropertyKind::Accessor {
          set: Value::Undefined,
          ..
        }
      ),
      "expected TextEncoder.prototype.encoding to be a read-only accessor property"
    );

    let te_tag_desc = scope
      .heap()
      .object_get_own_property(te_proto_obj, &to_string_tag_key)?
      .ok_or(VmError::InvariantViolation(
        "TextEncoder.prototype @@toStringTag missing",
      ))?;
    let PropertyKind::Data {
      value: Value::String(te_tag),
      writable: false,
    } = te_tag_desc.kind
    else {
      return Err(VmError::InvariantViolation(
        "TextEncoder @@toStringTag must be a non-writable data property",
      ));
    };
    assert_eq!(
      get_string(scope.heap(), Value::String(te_tag)),
      "TextEncoder"
    );

    let encoder =
      vm.construct_without_host(&mut scope, te_ctor, &[], Value::Object(te_ctor_obj))?;
    let Value::Object(encoder_obj) = encoder else {
      return Err(VmError::InvariantViolation(
        "TextEncoder constructor must return object",
      ));
    };
    assert!(
      scope
        .heap()
        .object_get_own_property(encoder_obj, &encoding_key)?
        .is_none(),
      "TextEncoder instances should not have own encoding property"
    );
    let enc_value = vm.get(&mut scope, encoder_obj, encoding_key)?;
    assert_eq!(get_string(scope.heap(), enc_value), "utf-8");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn text_decoder_stream_is_installed_and_constructable() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    let ty = rt.exec_script("typeof TextDecoderStream")?;
    assert_eq!(get_string(rt.heap(), ty), "function");

    let ok = rt.exec_script(
      "(() => { try { new TextDecoderStream(); return true; } catch { return false; } })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn text_decoder_stream_constructs_with_object_label() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    let ok = rt.exec_script(
      "(() => { new TextDecoderStream({ toString(){ return 'utf-8'; } }); return true; })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn text_decoder_stream_decodes_utf8_bytes_via_pipe_through() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    rt.exec_script(
      r#"
globalThis.__result = null;
globalThis.__error = null;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([104, 101, 108])); // "hel"
      controller.enqueue(new Uint8Array([108, 111])); // "lo"
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream());
  const reader = decoded.getReader();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += value;
  }
  return out;
})().then((v) => { globalThis.__result = v; }, (e) => { globalThis.__error = e; });
"#,
    )?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let err = rt.exec_script("globalThis.__error")?;
    if err != Value::Null {
      let err_msg = rt.exec_script("globalThis.__error && String(globalThis.__error)")?;
      panic!(
        "expected no error, got {err:?} ({})",
        get_string(rt.heap(), err_msg)
      );
    }
    let out = rt.exec_script("globalThis.__result")?;
    assert_eq!(get_string(rt.heap(), out), "hello");
    Ok(())
  }

  #[test]
  fn text_decoder_stream_decodes_data_view_chunks_via_pipe_through() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    rt.exec_script(
      r#"
globalThis.__result = null;
globalThis.__error = null;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      const buf = new Uint8Array([0, 104, 101, 108, 108, 111, 0]).buffer;
      controller.enqueue(new DataView(buf, 1, 5)); // "hello"
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream());
  const reader = decoded.getReader();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += value;
  }
  return out;
})().then((v) => { globalThis.__result = v; }, (e) => { globalThis.__error = e; });
"#,
    )?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let err = rt.exec_script("globalThis.__error")?;
    if err != Value::Null {
      let err_msg = rt.exec_script("globalThis.__error && String(globalThis.__error)")?;
      panic!(
        "expected no error, got {err:?} ({})",
        get_string(rt.heap(), err_msg)
      );
    }
    let out = rt.exec_script("globalThis.__result")?;
    assert_eq!(get_string(rt.heap(), out), "hello");
    Ok(())
  }

  #[test]
  fn text_decoder_stream_preserves_partial_multibyte_utf8_sequences_across_chunks() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    rt.exec_script(
      r#"
globalThis.__result = null;
globalThis.__error = null;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([0xE2, 0x82])); // partial "€"
      controller.enqueue(new Uint8Array([0xAC])); // completes "€"
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream());
  const reader = decoded.getReader();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += value;
  }
  return out;
})().then((v) => { globalThis.__result = v; }, (e) => { globalThis.__error = e; });
"#,
    )?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let err = rt.exec_script("globalThis.__error")?;
    assert_eq!(err, Value::Null, "expected no error, got {err:?}");
    let out = rt.exec_script("globalThis.__result")?;
    assert_eq!(get_string(rt.heap(), out), "€");
    Ok(())
  }

  #[test]
  fn text_decoder_stream_fatal_mode_errors_on_invalid_utf8() -> Result<(), VmError> {
    let (mut rt, _bindings) = new_runtime_with_streams_and_text_encoding()?;

    rt.exec_script(
      r#"
globalThis.__done = false;
globalThis.__threw = false;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([0xFF])); // invalid UTF-8
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream("utf-8", { fatal: true }));
  const reader = decoded.getReader();
  try {
    while (true) {
      const { done } = await reader.read();
      if (done) break;
    }
  } catch (e) {
    globalThis.__threw = true;
  }
  globalThis.__done = true;
})();
"#,
    )?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let done = rt.exec_script("globalThis.__done")?;
    assert_eq!(done, Value::Bool(true));
    let threw = rt.exec_script("globalThis.__threw")?;
    assert_eq!(threw, Value::Bool(true));
    Ok(())
  }
}
