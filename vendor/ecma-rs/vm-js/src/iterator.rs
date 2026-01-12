use crate::property::PropertyKey;
use crate::{GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript "IteratorRecord" (ECMA-262).
#[derive(Debug, Clone, Copy)]
pub struct IteratorRecord {
  pub iterator: Value,
  pub next_method: Value,
  pub done: bool,
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let key_s = scope.alloc_string(s)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
}

/// `GetIterator` (ECMA-262).
pub fn get_iterator(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
) -> Result<IteratorRecord, VmError> {
  // Iterator protocol: `GetMethod(iterable, @@iterator)`.
  let iterator_sym = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
    .well_known_symbols()
    .iterator;
  let Some(method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    iterable,
    PropertyKey::from_symbol(iterator_sym),
  )?
  else {
    return Err(VmError::TypeError("GetIterator: value is not iterable"));
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
    return Err(VmError::TypeError(
      "GetIteratorFromMethod: iterator method did not return an object",
    ));
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
    return Err(VmError::TypeError(
      "GetIteratorFromMethod: iterator.next is not callable",
    ));
  }

  Ok(IteratorRecord {
    iterator,
    next_method: next,
    done: false,
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
  vm.call_with_host_and_hooks(host, scope, hooks, record.next_method, record.iterator, &[])
}

/// `IteratorComplete` (ECMA-262).
pub fn iterator_complete(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iter_result: Value,
) -> Result<bool, VmError> {
  // Root the iterator result object across key allocation and `Get`, which can allocate/GC.
  let mut complete_scope = scope.reborrow();
  complete_scope.push_root(iter_result)?;

  let Value::Object(obj) = iter_result else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      &mut complete_scope,
      intr,
      "IteratorComplete: iterator result is not an object",
    ));
  };
  let done_key = string_key(&mut complete_scope, "done")?;
  let done = complete_scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    done_key,
    iter_result,
  )?;
  complete_scope.heap().to_boolean(done)
}

/// `IteratorValue` (ECMA-262).
pub fn iterator_value(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iter_result: Value,
) -> Result<Value, VmError> {
  // Root the iterator result object across key allocation and `Get`, which can allocate/GC.
  let mut value_scope = scope.reborrow();
  value_scope.push_root(iter_result)?;

  let Value::Object(obj) = iter_result else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      &mut value_scope,
      intr,
      "IteratorValue: iterator result is not an object",
    ));
  };
  let value_key = string_key(&mut value_scope, "value")?;
  value_scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, value_key, iter_result)
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

  let result = iterator_next(vm, host, hooks, scope, record)?;
  if iterator_complete(vm, host, hooks, scope, result)? {
    record.done = true;
    return Ok(None);
  }
  Ok(Some(iterator_value(vm, host, hooks, scope, result)?))
}

/// `IteratorClose` (ECMA-262).
///
/// This is used by `for..of` and iterator-consuming builtins to close iterators on abrupt
/// completion.
pub fn iterator_close(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &IteratorRecord,
) -> Result<(), VmError> {
  // Root the iterator across property-key allocation and the `GetMethod`/`Call` sequence. Without
  // this, the iterator object could be collected while closing (since values on the Rust stack are
  // not traced by the GC).
  let mut close_scope = scope.reborrow();
  close_scope.push_root(record.iterator)?;

  let return_key = string_key(&mut close_scope, "return")?;
  let Some(return_method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut close_scope,
    host,
    hooks,
    record.iterator,
    return_key,
  )?
  else {
    return Ok(());
  };

  close_scope.push_root(return_method)?;
  let result =
    vm.call_with_host_and_hooks(host, &mut close_scope, hooks, return_method, record.iterator, &[])?;

  if !matches!(result, Value::Object(_)) {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      &mut close_scope,
      intr,
      "IteratorClose: iterator.return did not return an object",
    ));
  }
  Ok(())
}

/// Native call handler for the `unwrap` closure in `AsyncFromSyncIteratorContinuation`.
///
/// This is used as the `onFulfilled` Promise reaction handler when awaiting an iterator result's
/// `value`.
///
/// Slot layout:
/// - slot 0: `done` boolean
pub(crate) fn async_from_sync_iterator_unwrap_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let done = match scope
    .heap()
    .get_function_native_slots(callee)?
    .get(0)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Bool(b) => b,
    _ => return Err(VmError::InvariantViolation("AsyncFromSyncIterator unwrap missing done slot")),
  };
  let v = args.get(0).copied().unwrap_or(Value::Undefined);

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;

  // Root inputs across allocations while constructing the iterator result object.
  let mut scope = scope.reborrow();
  scope.push_root(v)?;

  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;

  let value_key = string_key(&mut scope, "value")?;
  let done_key = string_key(&mut scope, "done")?;
  crate::spec_ops::create_data_property_or_throw(&mut scope, out, value_key, v)?;
  crate::spec_ops::create_data_property_or_throw(&mut scope, out, done_key, Value::Bool(done))?;

  Ok(Value::Object(out))
}

/// Native call handler for the `closeIterator` closure in `AsyncFromSyncIteratorContinuation`.
///
/// This is used as the `onRejected` Promise reaction handler when awaiting an iterator result's
/// `value`, ensuring the underlying sync iterator is closed if the value is a rejected promise.
///
/// Slot layout:
/// - slot 0: `syncIteratorRecord.[[Iterator]]` value
pub(crate) fn async_from_sync_iterator_close_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let iterator = scope
    .heap()
    .get_function_native_slots(callee)?
    .get(0)
    .copied()
    .unwrap_or(Value::Undefined);
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);

  // Root the iterator + reason across the IteratorClose call, which can allocate / run user JS.
  let mut scope = scope.reborrow();
  scope.push_roots(&[iterator, reason])?;

  let record = IteratorRecord {
    iterator,
    next_method: Value::Undefined,
    done: false,
  };

  iterator_close(vm, host, hooks, &mut scope, &record)?;
  Err(VmError::Throw(reason))
}
