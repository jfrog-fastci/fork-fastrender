use crate::{GcObject, GcString, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript `ToPrimitive` hint / preferred type.
///
/// Spec: <https://tc39.es/ecma262/#sec-toprimitive>
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToPrimitiveHint {
  Default,
  String,
  Number,
}

impl ToPrimitiveHint {
  #[inline]
  fn as_str(self) -> &'static str {
    match self {
      ToPrimitiveHint::Default => "default",
      ToPrimitiveHint::String => "string",
      ToPrimitiveHint::Number => "number",
    }
  }
}

impl<'a> Scope<'a> {
  /// ECMAScript `ToPrimitive(input, preferredType)`.
  ///
  /// This operation can invoke user code (`@@toPrimitive`, `valueOf`, `toString`) and therefore
  /// requires a [`Vm`] + host context.
  ///
  /// Note: the returned [`Value`] is **not automatically rooted**. Callers must root it if they
  /// will perform any further allocations that could trigger GC.
  pub fn to_primitive(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    input: Value,
    preferred_type: ToPrimitiveHint,
  ) -> Result<Value, VmError> {
    let Value::Object(obj) = input else {
      return Ok(input);
    };

    // Root `obj` across property lookups / calls (which may allocate and trigger GC).
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(obj))?;

    // 1. Let exoticToPrim be ? GetMethod(input, @@toPrimitive).
    let to_prim_sym = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
      .well_known_symbols()
      .to_primitive;
    let to_prim_key = PropertyKey::from_symbol(to_prim_sym);

    // `GetMethod` uses `GetV`/`ToObject`. Here `input` is already an object.
    let exotic = scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      obj,
      to_prim_key,
      Value::Object(obj),
    )?;

    // 2. If exoticToPrim is not undefined, then
    if !matches!(exotic, Value::Undefined | Value::Null) {
      if !scope.heap().is_callable(exotic)? {
        return Err(VmError::TypeError("@@toPrimitive is not callable"));
      }

      // 2.a. Let hint be "default"/"string"/"number".
      let hint_s = scope.alloc_string(preferred_type.as_str())?;
      scope.push_root(Value::String(hint_s))?;

      // 2.b. Let result be ? Call(exoticToPrim, input, « hint »).
      let result = vm.call_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        exotic,
        Value::Object(obj),
        &[Value::String(hint_s)],
      )?;

      // 2.c. If result is not an Object, return result.
      if !matches!(result, Value::Object(_)) {
        return Ok(result);
      }
      // 2.d. Throw a TypeError exception.
      return Err(VmError::TypeError("Cannot convert object to primitive value"));
    }

    // 3. If preferredType is not provided, let preferredType be Number.
    // 4. Return ? OrdinaryToPrimitive(input, preferredType).
    scope.ordinary_to_primitive(vm, host, hooks, obj, preferred_type)
  }

  /// ECMAScript `OrdinaryToPrimitive(O, hint)`.
  ///
  /// Spec: <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
  pub fn ordinary_to_primitive(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    hint: ToPrimitiveHint,
  ) -> Result<Value, VmError> {
    // Per spec, `hint` is either "string" or "number". For `ToPrimitive` callers passing "default",
    // treat it as "number".
    let hint = match hint {
      ToPrimitiveHint::Default => ToPrimitiveHint::Number,
      other => other,
    };
    let method_names = match hint {
      ToPrimitiveHint::String => ["toString", "valueOf"],
      ToPrimitiveHint::Number | ToPrimitiveHint::Default => ["valueOf", "toString"],
    };

    // Root `obj` across allocations for property key creation and calls.
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(obj))?;

    for name in method_names {
      let key_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let method = scope.ordinary_get_with_host_and_hooks(
        vm,
        host,
        hooks,
        obj,
        key,
        Value::Object(obj),
      )?;

      if matches!(method, Value::Undefined | Value::Null) {
        continue;
      }
      if !scope.heap().is_callable(method)? {
        continue;
      }

      let result = vm.call_with_host_and_hooks(host, &mut scope, hooks, method, Value::Object(obj), &[])?;
      if !matches!(result, Value::Object(_)) {
        return Ok(result);
      }
    }

    Err(VmError::TypeError("Cannot convert object to primitive value"))
  }

  /// ECMAScript `ToString(argument)` (partial).
  ///
  /// This implements object-to-string coercion using `ToPrimitive(argument, hint String)`.
  pub fn to_string(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<GcString, VmError> {
    if let Value::String(s) = value {
      return Ok(s);
    }

    let mut scope = self.reborrow();
    scope.push_root(value)?;

    let prim = match value {
      Value::Object(_) => scope.to_primitive(vm, host, hooks, value, ToPrimitiveHint::String)?,
      other => other,
    };
    scope.push_root(prim)?;
    debug_assert!(!matches!(prim, Value::Object(_)), "ToPrimitive returned object");

    scope.heap_mut().to_string(prim)
  }

  /// ECMAScript `ToNumber(argument)` (partial).
  ///
  /// This implements object-to-number coercion using `ToPrimitive(argument, hint Number)`.
  pub fn to_number(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<f64, VmError> {
    if let Value::Number(n) = value {
      return Ok(n);
    }

    let mut scope = self.reborrow();
    scope.push_root(value)?;

    let prim = match value {
      Value::Object(_) => scope.to_primitive(vm, host, hooks, value, ToPrimitiveHint::Number)?,
      other => other,
    };
    scope.push_root(prim)?;
    debug_assert!(!matches!(prim, Value::Object(_)), "ToPrimitive returned object");

    scope.heap_mut().to_number(prim)
  }

  /// ECMAScript `ToObject(argument)`.
  ///
  /// This performs `RequireObjectCoercible(argument)` (throwing for `null` / `undefined`) and boxes
  /// primitives into their corresponding wrapper objects.
  ///
  /// Note: this operation does **not** invoke user code, but it can allocate and therefore
  /// potentially trigger GC.
  pub fn to_object(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<GcObject, VmError> {
    match value {
      Value::Object(obj) => Ok(obj),
      Value::Undefined | Value::Null => Err(VmError::TypeError(
        "Cannot convert undefined or null to object",
      )),
      other => {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let object_ctor = Value::Object(intr.object_constructor());

        // Root the primitive value and callee across the internal boxing call (which can allocate).
        let mut scope = self.reborrow();
        scope.push_roots(&[other, object_ctor])?;

        let args = [other];
        let boxed = vm.call_with_host_and_hooks(host, &mut scope, hooks, object_ctor, Value::Undefined, &args)?;
        match boxed {
          Value::Object(obj) => Ok(obj),
          _ => Err(VmError::InvariantViolation(
            "ToObject internal boxing returned non-object",
          )),
        }
      }
    }
  }

  /// ECMAScript `ToPropertyKey(argument)`.
  ///
  /// This performs `ToPrimitive(argument, hint String)` followed by:
  /// - returning the Symbol directly, or
  /// - converting the primitive to a String.
  ///
  /// This operation can invoke user code (`@@toPrimitive`, `toString`, `valueOf`) and therefore
  /// requires a [`Vm`] + host context.
  pub fn to_property_key(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<PropertyKey, VmError> {
    // Root the input across `ToPrimitive` / `ToString` allocations, since both can trigger GC.
    let mut scope = self.reborrow();
    scope.push_root(value)?;

    let prim = match value {
      Value::Object(_) => scope.to_primitive(vm, host, hooks, value, ToPrimitiveHint::String)?,
      other => other,
    };
    scope.push_root(prim)?;
    debug_assert!(!matches!(prim, Value::Object(_)), "ToPrimitive returned object");

    match prim {
      Value::Symbol(sym) => Ok(PropertyKey::Symbol(sym)),
      other => {
        let s = scope.to_string(vm, host, hooks, other)?;
        Ok(PropertyKey::String(s))
      }
    }
  }
}
