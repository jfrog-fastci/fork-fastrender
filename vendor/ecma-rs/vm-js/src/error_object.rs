use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::source::{format_stack_trace, StackFrame};
use crate::{GcObject, Scope, Value, VmError};
use crate::{Intrinsics};

const MAX_ERROR_STACK_PROPERTY_FRAMES: usize = 64;

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

fn format_stack_property_string_best_effort(stack: &[StackFrame]) -> String {
  let frames = &stack[..stack.len().min(MAX_ERROR_STACK_PROPERTY_FRAMES)];
  format_stack_trace(frames)
}

/// Attach a non-standard `stack` own property to a thrown value.
///
/// This is best-effort: failure to allocate the stack string must not alter language-visible throw
/// semantics (the original value is still thrown/caught).
///
/// Policy: we attach `stack` to **any thrown object**, not only branded Error instances. For
/// all objects, the stack string contains only formatted stack frames. We never overwrite an
/// existing own `stack` property.
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

  let stack_string = format_stack_property_string_best_effort(stack);
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
