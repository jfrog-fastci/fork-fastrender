use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::source::{format_stack_trace, StackFrame};
use crate::{GcObject, GcString, Heap, Scope, Value, VmError};
use crate::{Intrinsics};

const MAX_ERROR_STACK_PROPERTY_FRAMES: usize = 64;
// Bound the total UTF-8 bytes of the formatted `error.stack` string to avoid attacker-controlled
// allocations. This is intentionally shared with other host-facing error formatting limits.
const MAX_ERROR_STACK_PROPERTY_BYTES: usize = crate::fallible_format::MAX_ERROR_MESSAGE_BYTES;
// Reserve a portion of the stack string budget for the `Error: message` header so frames are still
// visible even when the message is very large.
const MAX_ERROR_STACK_HEADER_BYTES: usize = 1024;

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

/// Create a minimal native `Error` object instance.
///
/// This is intentionally small and spec-shaped:
/// - Allocate an Error object allocation (branded so `Object.prototype.toString` builtin-tag
///   selection can distinguish real Error instances from `Object.create(Error.prototype)`).
/// - Set its `[[Prototype]]` to the provided intrinsic prototype.
/// - Define own non-enumerable `"name"` and `"message"` data properties.
pub fn new_error(
  scope: &mut Scope<'_>,
  prototype: GcObject,
  name: &str,
  message: &str,
) -> Result<Value, VmError> {
  let err = scope.alloc_error()?;
  // Root the object for the remainder of construction. Subsequent property definition may
  // allocate and trigger GC.
  scope.push_root(Value::Object(err))?;

  scope
    .heap_mut()
    .object_set_prototype(err, Some(prototype))?;

  let name_value = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_value))?;

  let message_value = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_value))?;

  // Root property keys: `define_property` can allocate and trigger GC, and GC does not see Rust
  // stack locals unless they are in the root set.
  let name_key_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(name_key_s))?;
  let name_key = PropertyKey::from_string(name_key_s);
  scope.define_property(err, name_key, data_desc(Value::String(name_value)))?;

  let message_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(message_key_s))?;
  let message_key = PropertyKey::from_string(message_key_s);
  scope.define_property(err, message_key, data_desc(Value::String(message_value)))?;

  Ok(Value::Object(err))
}

/// Allocates a new ECMAScript `TypeError` object (instance).
///
/// This is an object factory (not a callable constructor) intended for spec-shaped algorithms such
/// as module loading that need to reject/throw with real Error instances.
pub fn new_type_error_object(
  scope: &mut Scope<'_>,
  intrinsics: &Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_error(
    scope,
    intrinsics.type_error_prototype(),
    "TypeError",
    message,
  )
}

/// Allocates a new ECMAScript `SyntaxError` object (instance).
///
/// This is an object factory (not a callable constructor) intended for spec-shaped algorithms such
/// as module loading that need to reject/throw with real Error instances.
pub fn new_syntax_error_object(
  scope: &mut Scope<'_>,
  intrinsics: &Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_error(
    scope,
    intrinsics.syntax_error_prototype(),
    "SyntaxError",
    message,
  )
}

/// Allocates a new ECMAScript `RangeError` object (instance).
///
/// This is an object factory (not a callable constructor) intended for spec-shaped algorithms and
/// VM-internal helpers that need to reject/throw with real Error instances.
pub fn new_range_error_object(
  scope: &mut Scope<'_>,
  intrinsics: &Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_error(
    scope,
    intrinsics.range_error_prototype(),
    "RangeError",
    message,
  )
}

pub fn new_type_error(
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_type_error_object(scope, &intr, message)
}

pub fn throw_type_error(scope: &mut Scope<'_>, intr: Intrinsics, message: &str) -> VmError {
  match new_type_error(scope, intr, message) {
    Ok(value) => VmError::Throw(value),
    Err(err) => err,
  }
}

pub fn new_reference_error(
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_error(
    scope,
    intr.reference_error_prototype(),
    "ReferenceError",
    message,
  )
}

pub fn new_range_error(
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  message: &str,
) -> Result<Value, VmError> {
  new_range_error_object(scope, &intr, message)
}

pub fn throw_range_error(scope: &mut Scope<'_>, intr: Intrinsics, message: &str) -> VmError {
  match new_range_error(scope, intr, message) {
    Ok(value) => VmError::Throw(value),
    Err(err) => err,
  }
}

fn get_data_string_property_from_chain(
  heap: &Heap,
  start: GcObject,
  key: &PropertyKey,
) -> Option<GcString> {
  let mut current = Some(start);
  let mut steps = 0usize;
  while let Some(obj) = current {
    if steps >= crate::heap::MAX_PROTOTYPE_CHAIN {
      return None;
    }
    steps += 1;

    match heap.object_get_own_property(obj, key) {
      Ok(Some(desc)) => match desc.kind {
        PropertyKind::Data {
          value: Value::String(s),
          ..
        } => return Some(s),
        // If an accessor is present, or a non-string data property shadows the prototype chain, we
        // cannot safely format without invoking user code (or implementing full `ToString`), so
        // bail.
        PropertyKind::Accessor { .. } | PropertyKind::Data { .. } => return None,
      },
      Ok(None) => current = heap.object_prototype(obj).ok().flatten(),
      Err(_) => return None,
    }
  }
  None
}

fn format_error_stack_header_best_effort(
  heap: &Heap,
  obj: GcObject,
  name_key: &PropertyKey,
  message_key: &PropertyKey,
) -> Option<String> {
  fn string_from_str_best_effort(s: &str) -> String {
    let mut out = String::new();
    if out.try_reserve_exact(s.len()).is_ok() {
      out.push_str(s);
    }
    out
  }

  // Convert a JS string into bounded UTF-8.
  let to_utf8 = |s: GcString, max_bytes: usize| -> Option<String> {
    let js = heap.get_string(s).ok()?;
    crate::string::utf16_to_utf8_lossy_bounded(js.as_code_units(), max_bytes)
      .ok()
      .map(|(s, _truncated)| s)
  };

  // Spec-like defaults: missing `name` => "Error"; missing `message` => "".
  let name_value = get_data_string_property_from_chain(heap, obj, name_key);
  let message_value = get_data_string_property_from_chain(heap, obj, message_key);

  let name = match name_value {
    Some(s) => to_utf8(s, MAX_ERROR_STACK_HEADER_BYTES).unwrap_or_else(|| string_from_str_best_effort("Error")),
    None => string_from_str_best_effort("Error"),
  };
  let message = match message_value {
    Some(s) => to_utf8(s, MAX_ERROR_STACK_HEADER_BYTES).unwrap_or_default(),
    None => String::new(),
  };

  if name.is_empty() {
    return Some(message);
  }
  if message.is_empty() {
    return Some(name);
  }

  // `name + ": " + message`, bounded.
  let mut out = String::new();
  // Best-effort preallocation. If it fails, we still attempt incremental fallible writes below.
  let estimate = name
    .len()
    .saturating_add(2)
    .saturating_add(message.len())
    .min(MAX_ERROR_STACK_HEADER_BYTES);
  let _ = out.try_reserve_exact(estimate);

  // Use the same bounded, fallible helpers as stack-frame formatting so we never allocate
  // unbounded strings or abort the process under allocator OOM.
  let _ = try_push_str_limited(&mut out, &name, MAX_ERROR_STACK_HEADER_BYTES);
  let _ = try_push_str_limited(&mut out, ": ", MAX_ERROR_STACK_HEADER_BYTES);
  let _ = try_push_str_limited(&mut out, &message, MAX_ERROR_STACK_HEADER_BYTES);
  Some(out)
}

fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
  if s.len() <= max_bytes {
    return s;
  }
  let mut end = max_bytes;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  &s[..end]
}

fn try_push_str_limited(out: &mut String, s: &str, max_bytes: usize) -> Result<(), ()> {
  if out.len() >= max_bytes {
    return Ok(());
  }
  let remaining = max_bytes - out.len();
  let part = truncate_to_char_boundary(s, remaining);
  if part.is_empty() {
    return Ok(());
  }
  out.try_reserve(part.len()).map_err(|_| ())?;
  out.push_str(part);
  Ok(())
}

fn try_push_char_limited(out: &mut String, ch: char, max_bytes: usize) -> Result<(), ()> {
  if out.len() >= max_bytes {
    return Ok(());
  }
  let mut buf = [0u8; 4];
  let encoded = ch.encode_utf8(&mut buf);
  let remaining = max_bytes - out.len();
  if encoded.len() > remaining {
    return Ok(());
  }
  out.try_reserve(encoded.len()).map_err(|_| ())?;
  out.push(ch);
  Ok(())
}

fn format_stack_property_string_best_effort(
  heap: &Heap,
  obj: GcObject,
  stack: &[StackFrame],
  name_key: &PropertyKey,
  message_key: &PropertyKey,
) -> String {
  let frames = &stack[..stack.len().min(MAX_ERROR_STACK_PROPERTY_FRAMES)];
  let trace = format_stack_trace(frames);

  let mut out = String::new();
  let _ = out.try_reserve(trace.len().min(MAX_ERROR_STACK_PROPERTY_BYTES));

  if heap.is_error_object(obj) {
    if let Some(header) = format_error_stack_header_best_effort(heap, obj, name_key, message_key) {
      let _ = try_push_str_limited(&mut out, &header, MAX_ERROR_STACK_PROPERTY_BYTES);
    }
  }

  if !trace.is_empty() {
    if !out.is_empty() {
      let _ = try_push_char_limited(&mut out, '\n', MAX_ERROR_STACK_PROPERTY_BYTES);
    }
    let _ = try_push_str_limited(&mut out, &trace, MAX_ERROR_STACK_PROPERTY_BYTES);
  }

  out
}

/// Attach a non-standard `stack` own property to a thrown value.
///
/// This is best-effort: failure to allocate the stack string must not alter language-visible throw
/// semantics (the original value is still thrown/caught).
///
/// Policy: we attach `stack` to **any thrown object**, not only branded Error instances. For
/// non-Error objects, the stack string contains only formatted stack frames (no `"Error: message"`
/// header). We never overwrite an existing own `stack` property.
pub(crate) fn attach_stack_property_for_throw(
  scope: &mut Scope<'_>,
  value: Value,
  stack: &[StackFrame],
) {
  let Value::Object(obj) = value else {
    return;
  };

  // Best-effort root the object across allocations.
  let mut scope = scope.reborrow();
  if scope.push_root(Value::Object(obj)).is_err() {
    return;
  }

  // Pre-allocate and root commonly used keys; if any allocation fails, skip stack attachment.
  let Ok(stack_key_s) = scope.alloc_string("stack") else {
    return;
  };
  if scope.push_root(Value::String(stack_key_s)).is_err() {
    return;
  }
  let stack_key = PropertyKey::from_string(stack_key_s);

  // Don't overwrite an existing own `stack` property; user code may set custom stack strings.
  match scope.heap().object_get_own_property(obj, &stack_key) {
    Ok(Some(_)) => return,
    Ok(None) => {}
    Err(_) => return,
  }

  // `Error.stack` formatting needs `"name"`/`"message"` keys. We use the cached common keys to
  // avoid repeated allocations in typical throw/catch loops.
  let Ok(name_key_s) = scope.common_key_name() else {
    return;
  };
  if scope.push_root(Value::String(name_key_s)).is_err() {
    return;
  }
  let Ok(message_key_s) = scope.common_key_message() else {
    return;
  };
  if scope.push_root(Value::String(message_key_s)).is_err() {
    return;
  }
  let name_key = PropertyKey::from_string(name_key_s);
  let message_key = PropertyKey::from_string(message_key_s);

  let stack_string = format_stack_property_string_best_effort(scope.heap(), obj, stack, &name_key, &message_key);
  if stack_string.is_empty() {
    return;
  }

  let Ok(stack_s) = scope.alloc_string(&stack_string) else {
    return;
  };
  if scope.push_root(Value::String(stack_s)).is_err() {
    return;
  }

  let _ = scope.define_property(obj, stack_key, data_desc(Value::String(stack_s)));
}
