use crate::property::PropertyKey;
use crate::{Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript "IteratorRecord" (ECMA-262).
#[derive(Debug, Clone, Copy)]
pub struct IteratorRecord {
  pub iterator: Value,
  pub next_method: Value,
  pub done: bool,
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  Ok(PropertyKey::from_string(scope.alloc_string(s)?))
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
