//! Minimal `TextEncoder` / `TextDecoder` bindings for `Window` realms.
//!
//! These APIs are widely used by real-world scripts (analytics, polyfills, fetch helpers).
//! FastRender currently provides a UTF-8-only implementation backed by `vm-js` `ArrayBuffer` /
//! `Uint8Array` primitives.

use std::char;

use encoding_rs::UTF_8;
use vm_js::{
  new_range_error, HostSlots, Intrinsics, NativeConstructId, NativeFunctionId, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

const TEXT_ENCODER_HOST_TAG: u64 = 0x5445_5854_454E_4344; // "TEXTENCD"
const TEXT_DECODER_HOST_TAG: u64 = 0x5445_5854_4445_4344; // "TEXTDECD"

const TEXT_DECODER_FLAG_FATAL: u64 = 1 << 0;
const TEXT_DECODER_FLAG_IGNORE_BOM: u64 = 1 << 1;

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

fn label_is_utf8(code_units: &[u16]) -> Result<bool, VmError> {
  let trimmed = trim_ascii_whitespace_units(code_units);
  if trimmed.is_empty() {
    return Ok(false);
  }
  if trimmed.len() > MAX_TEXT_DECODER_LABEL_CODE_UNITS {
    return Ok(false);
  }

  // Encoding labels are ASCII. Convert to lowercase bytes for `encoding_rs`.
  let mut bytes: Vec<u8> = Vec::new();
  bytes
    .try_reserve_exact(trimmed.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for &unit in trimmed {
    if unit > 0x7F {
      return Ok(false);
    }
    bytes.push((unit as u8).to_ascii_lowercase());
  }

  Ok(encoding_rs::Encoding::for_label(bytes.as_slice()) == Some(UTF_8))
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

fn decode_utf8_lossy_to_utf16_units(bytes: &[u8]) -> Result<Vec<u16>, VmError> {
  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve_exact(bytes.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut i = 0usize;
  while i < bytes.len() {
    match std::str::from_utf8(&bytes[i..]) {
      Ok(valid) => {
        for ch in valid.chars() {
          let mut buf = [0u16; 2];
          let encoded = ch.encode_utf16(&mut buf);
          out.extend_from_slice(encoded);
        }
        break;
      }
      Err(err) => {
        let valid_up_to = err.valid_up_to();
        if valid_up_to > 0 {
          let valid =
            unsafe { std::str::from_utf8_unchecked(&bytes[i..i.saturating_add(valid_up_to)]) };
          for ch in valid.chars() {
            let mut buf = [0u16; 2];
            let encoded = ch.encode_utf16(&mut buf);
            out.extend_from_slice(encoded);
          }
        }
        out.push(0xFFFD);
        let err_len = err.error_len().unwrap_or(1);
        i = i
          .checked_add(valid_up_to)
          .and_then(|v| v.checked_add(err_len))
          .ok_or(VmError::OutOfMemory)?;
      }
    }
  }

  Ok(out)
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

  // Validate encoding label.
  if let Some(label_value) = args.get(0).copied() {
    if !matches!(label_value, Value::Undefined) {
      let label_string = match label_value {
        Value::String(s) => s,
        other => scope.heap_mut().to_string(other)?,
      };
      let label_units = scope.heap().get_string(label_string)?.as_code_units();
      let ok = label_is_utf8(label_units)?;
      if !ok {
        return Err(VmError::Throw(new_range_error(
          &mut scope,
          intr,
          "The encoding label provided is invalid.",
        )?));
      }
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
      a: TEXT_DECODER_HOST_TAG,
      b: flags,
    },
  )?;

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
  callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let _ = require_text_decoder_receiver(scope, this)?;
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.first().copied() {
    Some(Value::String(s)) => Ok(Value::String(s)),
    _ => Err(VmError::InvariantViolation(
      "TextDecoder encoding getter missing utf-8 slot",
    )),
  }
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
  let (_obj, flags) = require_text_decoder_receiver(scope, this)?;
  Ok(Value::Bool((flags & TEXT_DECODER_FLAG_FATAL) != 0))
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
  let (_obj, flags) = require_text_decoder_receiver(scope, this)?;
  Ok(Value::Bool((flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0))
}

fn text_encoder_encode(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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
    other => scope.heap_mut().to_string(other)?,
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
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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
      other => scope.heap_mut().to_string(other)?,
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
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, flags) = require_text_decoder_receiver(scope, this)?;
  let ignore_bom = (flags & TEXT_DECODER_FLAG_IGNORE_BOM) != 0;
  let fatal = (flags & TEXT_DECODER_FLAG_FATAL) != 0;

  let input = args.get(0).copied().unwrap_or(Value::Undefined);

  match input {
    Value::Undefined => {
      let empty = scope.alloc_string("")?;
      Ok(Value::String(empty))
    }
    Value::Object(obj) => {
      let units = {
        let heap = scope.heap();
        let data = if heap.is_array_buffer_object(obj) {
          heap.array_buffer_data(obj)?
        } else if heap.is_uint8_array_object(obj) {
          heap.uint8_array_data(obj)?
        } else {
          return Err(VmError::TypeError(
            "TextDecoder.decode expects an ArrayBuffer or Uint8Array",
          ));
        };

        if data.len() > MAX_TEXT_DECODER_INPUT_BYTES {
          return Err(VmError::TypeError("TextDecoder input too large"));
        }

        let data = if !ignore_bom && data.starts_with(&[0xEF, 0xBB, 0xBF]) {
          &data[3..]
        } else {
          data
        };

        if fatal && std::str::from_utf8(data).is_err() {
          return Err(VmError::TypeError("The encoded data was not valid UTF-8"));
        }

        decode_utf8_lossy_to_utf16_units(data)?
      };

      let out = scope.alloc_string_from_u16_vec(units)?;
      Ok(Value::String(out))
    }
    _ => Err(VmError::TypeError(
      "TextDecoder.decode expects an ArrayBuffer or Uint8Array",
    )),
  }
}

pub(crate) fn install_window_text_encoding_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut vm_js::Heap,
) -> Result<(), VmError> {
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
  let td_encoding_get_slots = [Value::String(utf8_s)];
  let td_encoding_get_fn = scope.alloc_native_function_with_slots(
    td_encoding_get_call_id,
    None,
    td_encoding_get_name_s,
    0,
    &td_encoding_get_slots,
  )?;
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
}

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::{HeapLimits, VmOptions};

  fn get_string(heap: &vm_js::Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn text_encoder_utf8_encodes_strings() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

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
  fn text_decoder_utf8_decodes_uint8_array() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

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
  fn text_encoder_encode_into_writes_into_destination_and_returns_counts() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = vm_js::Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

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
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

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
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let decoder_ctor_key = alloc_key(&mut scope, "TextDecoder")?;
    let decoder_ctor = vm.get(&mut scope, global, decoder_ctor_key)?;
    let Value::Object(decoder_ctor_obj) = decoder_ctor else {
      return Err(VmError::InvariantViolation("TextDecoder missing"));
    };

    let bad_label = scope.alloc_string("utf-16")?;
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
    install_window_text_encoding_bindings(&mut vm, &realm, &mut heap)?;

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
}
