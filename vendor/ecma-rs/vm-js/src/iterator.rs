use crate::property::PropertyKey;
use crate::error_object::new_type_error_object;
use crate::{GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript "IteratorRecord" (ECMA-262).
///
/// This is intentionally spec-shaped (iterator object + next method + done flag). For now we also
/// embed a private fast-path state for Array iteration so `for..of`/spread can work before full
/// `%Array.prototype%[@@iterator]` exists.
#[derive(Debug, Clone, Copy)]
pub struct IteratorRecord {
  pub iterator: Value,
  pub next_method: Value,
  pub done: bool,
  kind: IteratorKind,
}

#[derive(Debug, Clone, Copy)]
enum IteratorKind {
  Protocol,
  Array {
    array: GcObject,
    next_index: u32,
    length: u32,
  },
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  Ok(PropertyKey::from_string(scope.alloc_string(s)?))
}

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let err = new_type_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(err))
}

fn is_array(scope: &mut Scope<'_>, value: Value) -> Result<Option<GcObject>, VmError> {
  let Value::Object(obj) = value else {
    return Ok(None);
  };
  if scope.heap().object_is_array(obj)? {
    return Ok(Some(obj));
  }
  Ok(None)
}

fn array_length(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  array: GcObject,
) -> Result<u32, VmError> {
  let length_key = string_key(scope, "length")?;
  let len_value =
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, array, length_key, Value::Object(array))?;
  match len_value {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n as u32),
    _ => Err(VmError::Unimplemented("Array length is not a uint32 Number")),
  }
}

fn get_method(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  obj: Value,
  key: PropertyKey,
) -> Result<Option<Value>, VmError> {
  // `GetMethod(V, P)` uses `GetV(V, P)`, which performs `ToObject(V)` for the property lookup but
  // still uses the original `V` as the `receiver`/`this` value for accessor getters.
  let mut scope = scope.reborrow();
  let (obj, receiver) = match obj {
    Value::Object(obj) => {
      scope.push_root(Value::Object(obj))?;
      (obj, Value::Object(obj))
    }
    Value::Null | Value::Undefined => {
      return Err(throw_type_error(
        vm,
        &mut scope,
        "GetMethod: cannot convert null/undefined to object",
      )?);
    }
    other => {
      // Root `other` across boxing + property access; for primitives like String, the receiver is
      // still the primitive value.
      scope.push_root(other)?;
      let wrapped_obj = scope.to_object(vm, host, hooks, other)?;
      scope.push_root(Value::Object(wrapped_obj))?;
      (wrapped_obj, other)
    }
  };

  let func = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)?;
  if matches!(func, Value::Undefined | Value::Null) {
    return Ok(None);
  }
  if !scope.heap().is_callable(func)? {
    return Err(throw_type_error(vm, &mut scope, "GetMethod: target is not callable")?);
  }
  Ok(Some(func))
}

/// `GetIterator` (ECMA-262).
///
/// For now, this supports:
/// - A fast path for Array exotic objects.
/// - A minimal iterator-protocol path via `@@iterator` for objects with native-callable iterator
///   methods.
pub fn get_iterator(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
) -> Result<IteratorRecord, VmError> {
  if let Some(array) = is_array(scope, iterable)? {
    let length = array_length(vm, host, hooks, scope, array)?;
    return Ok(IteratorRecord {
      iterator: Value::Object(array),
      next_method: Value::Undefined,
      done: false,
      kind: IteratorKind::Array {
        array,
        next_index: 0,
        length,
      },
    });
  }

  // Fall back to iterator protocol: `GetMethod(iterable, @@iterator)`.
  let iterator_sym = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
    .well_known_symbols()
    .iterator;
  let Some(method) =
    get_method(vm, host, hooks, scope, iterable, PropertyKey::from_symbol(iterator_sym))?
  else {
    return Err(throw_type_error(vm, scope, "GetIterator: value is not iterable")?);
  };
  get_iterator_from_method(vm, host, hooks, scope, iterable, method)
}

/// `GetIteratorFromMethod` (ECMA-262).
pub fn get_iterator_from_method(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
  method: Value,
) -> Result<IteratorRecord, VmError> {
  let iterator = vm.call_with_host_and_hooks(host, scope, hooks, method, iterable, &[])?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(throw_type_error(
      vm,
      scope,
      "GetIteratorFromMethod: iterator method did not return an object",
    )?);
  };

  // Root the iterator object while allocating/reading the `next` method in case those operations
  // trigger GC.
  let mut next_scope = scope.reborrow();
  next_scope.push_root(iterator)?;

  let next_key = string_key(&mut next_scope, "next")?;
  let next = next_scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    iterator_obj,
    next_key,
    Value::Object(iterator_obj),
  )?;
  if !next_scope.heap().is_callable(next)? {
    return Err(throw_type_error(
      vm,
      &mut next_scope,
      "GetIteratorFromMethod: iterator.next is not callable",
    )?);
  }

  Ok(IteratorRecord {
    iterator,
    next_method: next,
    done: false,
    kind: IteratorKind::Protocol,
  })
}

/// `IteratorNext` (ECMA-262).
pub fn iterator_next(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &IteratorRecord,
) -> Result<Value, VmError> {
  match record.kind {
    IteratorKind::Protocol => {
      vm.call_with_host_and_hooks(host, scope, hooks, record.next_method, record.iterator, &[])
    }
    IteratorKind::Array { .. } => Err(VmError::Unimplemented(
      "IteratorNext is not used for Array fast-path iterators",
    )),
  }
}

/// `IteratorComplete` (ECMA-262).
pub fn iterator_complete(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iter_result: Value,
) -> Result<bool, VmError> {
  let Value::Object(obj) = iter_result else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      scope,
      intr,
      "IteratorComplete: iterator result is not an object",
    ));
  };
  let done_key = string_key(scope, "done")?;
  let done = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, done_key, iter_result)?;
  scope.heap().to_boolean(done)
}

/// `IteratorValue` (ECMA-262).
pub fn iterator_value(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iter_result: Value,
) -> Result<Value, VmError> {
  let Value::Object(obj) = iter_result else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      scope,
      intr,
      "IteratorValue: iterator result is not an object",
    ));
  };
  let value_key = string_key(scope, "value")?;
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, value_key, iter_result)
}

/// `IteratorStepValue` (ECMA-262).
///
/// Returns `Ok(None)` when iteration is complete.
pub fn iterator_step_value(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &mut IteratorRecord,
) -> Result<Option<Value>, VmError> {
  if record.done {
    return Ok(None);
  }

  match &mut record.kind {
    IteratorKind::Array {
      array,
      next_index,
      length,
    } => {
      if *next_index >= *length {
        record.done = true;
        return Ok(None);
      }

      let idx = *next_index;
      *next_index = next_index.saturating_add(1);

      let key = string_key(scope, &idx.to_string())?;
      let value = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, *array, key, Value::Object(*array))?;
      Ok(Some(value))
    }
    IteratorKind::Protocol => {
      let result = iterator_next(vm, host, hooks, scope, record)?;
      if iterator_complete(vm, host, hooks, scope, result)? {
        record.done = true;
        return Ok(None);
      }
      Ok(Some(iterator_value(vm, host, hooks, scope, result)?))
    }
  }
}

/// `IteratorClose` (ECMA-262) (best-effort).
///
/// This is used by `for..of` to close iterators on abrupt completion. For now we intentionally
/// swallow any error from the `return` call since the surrounding interpreter does not yet have a
/// full exception model.
pub fn iterator_close(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &IteratorRecord,
) -> Result<(), VmError> {
  if matches!(record.kind, IteratorKind::Array { .. }) {
    return Ok(());
  }

  let return_key = string_key(scope, "return")?;
  let Some(return_method) = get_method(vm, host, hooks, scope, record.iterator, return_key)? else {
    return Ok(());
  };

  // Best-effort: ignore errors.
  let _ = vm.call_with_host_and_hooks(host, scope, hooks, return_method, record.iterator, &[]);
  Ok(())
}

// Note: old `vm-js.internal.ArrayMarker` tagging was removed now that arrays are proper exotic
// objects.
