use crate::{
  function::CallHandler, GcObject, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

fn define_internal_symbol_property_or_throw(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
  desc: crate::PropertyDescriptorPatch,
) -> Result<(), VmError> {
  // This helper is only for hidden/internal symbol keys (class-private names, internal slots, ...).
  let PropertyKey::Symbol(sym) = key else {
    return Err(VmError::InvariantViolation(
      "internal symbol property definition with non-symbol key",
    ));
  };
  if !scope.heap().is_internal_symbol(sym) {
    return Err(VmError::InvariantViolation(
      "internal symbol property definition with non-internal symbol key",
    ));
  }

  // Private-name and internal-slot definitions are not observable and must not invoke Proxy traps.
  if !scope.heap().is_proxy_object(obj) {
    return scope.define_property_or_throw(obj, key, desc);
  }

  // Root `obj`, `key`, and any descriptor values for the duration of the operation.
  let mut scope = scope.reborrow();
  let mut roots = [Value::Undefined; 5];
  let mut root_count = 0usize;
  roots[root_count] = Value::Object(obj);
  root_count += 1;
  roots[root_count] = Value::Symbol(sym);
  root_count += 1;
  if let Some(v) = desc.value {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = desc.get {
    roots[root_count] = v;
    root_count += 1;
  }
  if let Some(v) = desc.set {
    roots[root_count] = v;
    root_count += 1;
  }
  scope.push_roots(&roots[..root_count])?;

  desc.validate()?;
  let enumerable = desc.enumerable.unwrap_or(false);
  let configurable = desc.configurable.unwrap_or(false);
  let desc = if desc.is_accessor_descriptor() {
    crate::PropertyDescriptor {
      enumerable,
      configurable,
      kind: crate::PropertyKind::Accessor {
        get: desc.get.unwrap_or(Value::Undefined),
        set: desc.set.unwrap_or(Value::Undefined),
      },
    }
  } else {
    crate::PropertyDescriptor {
      enumerable,
      configurable,
      kind: crate::PropertyKind::Data {
        value: desc.value.unwrap_or(Value::Undefined),
        writable: desc.writable.unwrap_or(false),
      },
    }
  };

  scope
    .heap_mut()
    .proxy_internal_define_own_property(obj, key, desc)?;
  Ok(())
}

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

/// Initialize per-instance elements for `class_ctor` on the already-constructed `receiver`.
///
/// This implements:
/// - public instance fields (`x = <expr>` / `x;`), and
/// - private instance elements defined on the class prototype (private methods / accessors), which
///   are modeled as internal symbol-keyed properties on the prototype and must be copied onto each
///   instance so private brand checks use own-property lookups.
///
/// Field records are stored as `(key, initializer)` pairs in the class constructor's native slots.
/// - `key` is stored as `Value::String` or `Value::Symbol`.
/// - For fields:
///   - `initializer` is stored as `Value::Object(func)` where `func` is a `ClassFieldInitializer`
///     function, or `Value::Undefined` for "no initializer".
/// - For private instance methods:
///   - `initializer` stores the method function object itself (`Value::Object(func)`), which must
///     *not* be invoked during initialization.
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

  // Initialize private instance accessors (and any other internal-symbol keyed elements defined on
  // the prototype).
  //
  // `vm-js` represents class-private names (`#x`, `#m`, ...) via internal symbols scoped to the
  // class's lexical environment. Private instance fields and private instance methods are stored
  // in the class constructor's native-slot element list, but private accessors are currently
  // defined on the class's prototype object during class definition evaluation.
  //
  // For spec-correct per-instance privacy (brand checks) these private methods/accessors must also
  // be installed as **own** properties on each constructed instance. Otherwise, `this.#m()` would
  // fail the private brand check in `Evaluator::private_get`, which intentionally does not consult
  // the prototype chain.
  //
  // Private methods are handled via the native-slot list below. For accessors, we copy any
  // internal-symbol keyed properties from the class prototype object onto the instance before
  // evaluating any field initializers. This matches the observable behavior of
  // `InitializeInstanceElements` where private methods/accessors are available to field
  // initializers.
  let prototype_obj = {
    let prototype_key = PropertyKey::from_string(init_scope.common_key_prototype()?);
    let Some(prototype_desc) = init_scope.heap().get_own_property(class_ctor, prototype_key)? else {
      return Err(VmError::InvariantViolation(
        "class constructor missing prototype property",
      ));
    };
    let crate::PropertyKind::Data { value, .. } = prototype_desc.kind else {
      return Err(VmError::InvariantViolation(
        "class constructor prototype property is not a data property",
      ));
    };
    let Value::Object(o) = value else {
      return Err(VmError::InvariantViolation(
        "class constructor prototype property is not an object",
      ));
    };
    o
  };
  init_scope.push_root(Value::Object(prototype_obj))?;

  let prototype_keys = init_scope.heap().own_property_keys(prototype_obj)?;
  for key in prototype_keys {
    let PropertyKey::Symbol(sym) = key else {
      continue;
    };
    if !init_scope.heap().is_internal_symbol(sym) {
      continue;
    }
    let Some(desc) = init_scope.heap().get_own_property(prototype_obj, key)? else {
      continue;
    };

    let patch = match desc.kind {
      crate::PropertyKind::Data { value, writable } => crate::PropertyDescriptorPatch {
        value: Some(value),
        writable: Some(writable),
        enumerable: Some(desc.enumerable),
        configurable: Some(desc.configurable),
        ..Default::default()
      },
      crate::PropertyKind::Accessor { get, set } => crate::PropertyDescriptorPatch {
        get: Some(get),
        set: Some(set),
        enumerable: Some(desc.enumerable),
        configurable: Some(desc.configurable),
        ..Default::default()
      },
    };

    define_internal_symbol_property_or_throw(&mut init_scope, receiver, key, patch)?;
  }

  // Copy the native-slot slice into an owned Vec so we can mutably borrow `init_scope` while
  // evaluating initializers and defining properties.
  let pairs_slice = class_constructor_instance_field_pairs(&init_scope, class_ctor)?;
  let pairs: Vec<Value> = if pairs_slice.is_empty() {
    Vec::new()
  } else {
    if pairs_slice.len() % 2 != 0 {
      return Err(VmError::InvariantViolation(
        "class constructor instance field list has odd length",
      ));
    }
    let mut out: Vec<Value> = Vec::new();
    out
      .try_reserve_exact(pairs_slice.len())
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(pairs_slice);
    out
  };

  // Private methods must be available to all field initializers, regardless of source order.
  //
  // vm-js stores private instance methods in the instance-element slot list as `(sym, func)` pairs,
  // interleaved with field records. Define those methods up-front so subsequent field initializer
  // evaluation can call `this.#m()` even when `#m` is declared later in the class body.
  for pair in pairs.chunks_exact(2) {
    let key_value = pair[0];
    let init_value = pair[1];

    let Value::Symbol(sym) = key_value else {
      continue;
    };
    if !init_scope.heap().is_internal_symbol(sym) {
      continue;
    }
    let Value::Object(func) = init_value else {
      continue;
    };

    // Skip field initializer functions (they are invoked in source order below).
    let is_field_init = match init_scope.heap().get_function(func) {
      Ok(f) => match &f.call {
        CallHandler::Ecma(code_id) => vm
          .ecma_function_source_span(*code_id)
          .map(|(_, _, _, kind)| kind == crate::vm::EcmaFunctionKind::ClassFieldInitializer)
          .unwrap_or(false),
        _ => false,
      },
      Err(_) => false,
    };
    if is_field_init {
      continue;
    }

    let key = PropertyKey::from_symbol(sym);
    let already_defined = if init_scope.heap().is_proxy_object(receiver) {
      init_scope
        .heap()
        .proxy_internal_get_own_property(receiver, &key)?
        .is_some()
    } else {
      init_scope.heap().get_own_property(receiver, key)?.is_some()
    };
    if already_defined {
      continue;
    }

    define_internal_symbol_property_or_throw(
      &mut init_scope,
      receiver,
      key,
      crate::PropertyDescriptorPatch {
        value: Some(Value::Object(func)),
        writable: Some(false),
        enumerable: Some(false),
        configurable: Some(false),
        ..Default::default()
      },
    )?;
  }

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

    let is_private = matches!(key, PropertyKey::Symbol(sym) if init_scope.heap().is_internal_symbol(sym));

    // Evaluate the initializer before defining the property (spec: `DefineField`).
    //
    // vm-js stores:
    // - field initializers as `ClassFieldInitializer` functions (must be invoked here), and
    // - private instance methods as the method function object itself (must *not* be invoked).
    let (value, is_field_initializer) = match init_value {
      Value::Object(func) => {
        let is_field_init = match init_scope.heap().get_function(func) {
          Ok(f) => match &f.call {
            CallHandler::Ecma(code_id) => vm
              .ecma_function_source_span(*code_id)
              .map(|(_, _, _, kind)| kind == crate::vm::EcmaFunctionKind::ClassFieldInitializer)
              .unwrap_or(false),
            _ => false,
          },
          Err(_) => false,
        };
        if is_field_init {
          let value = vm.call_with_host_and_hooks(
            host,
            &mut init_scope,
            hooks,
            Value::Object(func),
            Value::Object(receiver),
            &[],
          )?;
          (value, true)
        } else {
          // Private methods were defined in the pre-pass above.
          if is_private {
            continue;
          }
          (Value::Object(func), false)
        }
      }
      Value::Undefined => (Value::Undefined, true),
      _ => {
        return Err(VmError::InvariantViolation(
          "class constructor instance element initializer is not an object or undefined",
        ))
      }
    };

    init_scope.push_root(value)?;

    // Private elements are stored as internal-symbol-keyed properties so they are filtered out by
    // `[[OwnPropertyKeys]]` (`Object.getOwnPropertySymbols`, `Reflect.ownKeys`, ...).
    //
    // - Private fields are writable (like `DefineField`).
    // - Private instance methods are not writable (attempts to assign should throw).
    // - Both are non-enumerable and non-configurable.
    if is_private {
      let writable = is_field_initializer;
      define_internal_symbol_property_or_throw(
        &mut init_scope,
        receiver,
        key,
        crate::PropertyDescriptorPatch {
          value: Some(value),
          writable: Some(writable),
          enumerable: Some(false),
          configurable: Some(false),
          ..Default::default()
        },
      )?;
    } else {
      crate::spec_ops::create_data_property_or_throw_with_host_and_hooks(
        vm,
        &mut init_scope,
        host,
        hooks,
        receiver,
        key,
        value,
      )?;
    }
  }

  Ok(())
}
