use crate::{GcObject, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// Native slot index for a class constructor's hidden user-defined constructor body function.
pub(crate) const CLASS_CTOR_SLOT_BODY: usize = 0;
/// Native slot index for a class constructor's `extends` value.
///
/// - `undefined` => base class (no `extends`).
/// - `null` => `extends null`.
/// - `object` => base constructor.
pub(crate) const CLASS_CTOR_SLOT_SUPER: usize = 1;
/// Native slot index where instance field (key, initializer) pairs begin.
pub(crate) const CLASS_CTOR_SLOT_INSTANCE_FIELDS_START: usize = 2;

pub(crate) fn class_constructor_body(
  scope: &Scope<'_>,
  class_ctor: GcObject,
) -> Result<Option<GcObject>, VmError> {
  let slots = scope.heap().get_function_native_slots(class_ctor)?;
  match slots.get(CLASS_CTOR_SLOT_BODY).copied().unwrap_or(Value::Undefined) {
    Value::Object(o) => Ok(Some(o)),
    Value::Undefined => Ok(None),
    _ => Err(VmError::InvariantViolation(
      "class constructor body slot is not an object or undefined",
    )),
  }
}

pub(crate) fn class_constructor_super_value(
  scope: &Scope<'_>,
  class_ctor: GcObject,
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(class_ctor)?;
  Ok(
    slots
      .get(CLASS_CTOR_SLOT_SUPER)
      .copied()
      .unwrap_or(Value::Undefined),
  )
}

pub(crate) fn class_constructor_instance_field_pairs<'a>(
  scope: &'a Scope<'_>,
  class_ctor: GcObject,
) -> Result<&'a [Value], VmError> {
  let slots = scope.heap().get_function_native_slots(class_ctor)?;
  Ok(
    slots
      .get(CLASS_CTOR_SLOT_INSTANCE_FIELDS_START..)
      .unwrap_or(&[]),
  )
}

/// Initialize public instance fields for `class_ctor` on the already-constructed `receiver`.
///
/// This implements the public-field subset of `InitializeInstanceElements` / `DefineField`.
///
/// Field records are stored as `(key, initializer)` pairs in the class constructor's native slots.
/// - `key` is stored as `Value::String` or `Value::Symbol`.
/// - `initializer` is stored as `Value::Object(func)` or `Value::Undefined` for "no initializer".
pub(crate) fn initialize_instance_fields_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  receiver: GcObject,
  class_ctor: GcObject,
) -> Result<(), VmError> {
  let mut init_scope = scope.reborrow();
  init_scope.push_roots(&[Value::Object(receiver), Value::Object(class_ctor)])?;

  // Copy the native-slot slice into an owned Vec so we can mutably borrow `init_scope` while
  // evaluating initializers and defining properties.
  let pairs: Vec<Value> = {
    let pairs = class_constructor_instance_field_pairs(&init_scope, class_ctor)?;
    if pairs.is_empty() {
      return Ok(());
    }
    if pairs.len() % 2 != 0 {
      return Err(VmError::InvariantViolation(
        "class constructor instance field list has odd length",
      ));
    }
    let mut out: Vec<Value> = Vec::new();
    out
      .try_reserve_exact(pairs.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(pairs);
    out
  };

  for pair in pairs.chunks_exact(2) {
    let key_value = pair[0];
    let init_value = pair[1];

    let key = match key_value {
      Value::String(s) => PropertyKey::from_string(s),
      Value::Symbol(s) => PropertyKey::from_symbol(s),
      Value::Undefined => {
        return Err(VmError::InvariantViolation(
          "class constructor instance field key slot is undefined",
        ))
      }
      _ => {
        return Err(VmError::InvariantViolation(
          "class constructor instance field key is not a string or symbol",
        ))
      }
    };

    // Evaluate the initializer before defining the property (spec: `DefineField`).
    let value = match init_value {
      Value::Object(func) => vm.call_with_host_and_hooks(
        host,
        &mut init_scope,
        hooks,
        Value::Object(func),
        Value::Object(receiver),
        &[],
      )?,
      Value::Undefined => Value::Undefined,
      _ => {
        return Err(VmError::InvariantViolation(
          "class constructor instance field initializer is not a function or undefined",
        ))
      }
    };

    init_scope.push_root(value)?;
    // Private fields are stored as symbol-keyed properties using internal symbols so they are
    // filtered out by `[[OwnPropertyKeys]]` (`Object.getOwnPropertySymbols`, `Reflect.ownKeys`, ...).
    //
    // Unlike public fields, they must be non-enumerable and non-configurable.
    let is_private_field = matches!(key, PropertyKey::Symbol(sym) if init_scope.heap().is_internal_symbol(sym));
    if is_private_field {
      init_scope.define_property_or_throw(
        receiver,
        key,
        crate::PropertyDescriptorPatch {
          value: Some(value),
          writable: Some(true),
          enumerable: Some(false),
          configurable: Some(false),
          ..Default::default()
        },
      )?;
    } else {
      init_scope.create_data_property_or_throw(receiver, key, value)?;
    }
  }

  Ok(())
}
