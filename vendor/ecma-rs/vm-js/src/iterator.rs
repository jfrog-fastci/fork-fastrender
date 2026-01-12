use crate::property::PropertyKey;
use crate::{GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript "IteratorRecord" (ECMA-262).
#[derive(Debug, Clone, Copy)]
pub struct IteratorRecord {
  pub iterator: Value,
  pub next_method: Value,
  pub done: bool,
}

/// IteratorClose completion classification.
///
/// This mirrors the spec behavior of `IteratorClose(iteratorRecord, completion)`, but avoids
/// coupling the iterator layer to `exec.rs::Completion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCompletionKind {
  /// Closing on a *throw* completion (errors from `return` are suppressed).
  Throw,
  /// Closing on a *non-throw* completion (errors from `return` are propagated).
  NonThrow,
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let key_s = scope.alloc_string(s)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
}

fn get_iterator_via_protocol(
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

/// `GetIterator` (ECMA-262).
pub fn get_iterator(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
) -> Result<IteratorRecord, VmError> {
  get_iterator_via_protocol(vm, host, hooks, scope, iterable)
}

/// `GetIterator` (ECMA-262) via iterator protocol only (no internal fast paths).
///
/// This exists for spec-shaped callers (notably `yield*` delegation) that must receive an iterator
/// record with a callable `next_method`.
pub fn get_iterator_protocol(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
) -> Result<IteratorRecord, VmError> {
  get_iterator_via_protocol(vm, host, hooks, scope, iterable)
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
  let next = next_scope.get_with_host_and_hooks(
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
  record: &mut IteratorRecord,
  value: Option<Value>,
) -> Result<Value, VmError> {
  let args_buf;
  let args: &[Value] = match value {
    None => &[][..],
    Some(v) => {
      args_buf = [v];
      &args_buf[..]
    }
  };

  let result = match vm.call_with_host_and_hooks(host, scope, hooks, record.next_method, record.iterator, args) {
    Ok(v) => v,
    Err(err) => {
      record.done = true;
      return Err(err);
    }
  };

  if !matches!(result, Value::Object(_)) {
    record.done = true;
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    return Err(crate::throw_type_error(
      scope,
      intr,
      "IteratorNext: iterator result is not an object",
    ));
  }

  Ok(result)
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
  let done = complete_scope.get_with_host_and_hooks(vm, host, hooks, obj, done_key, iter_result)?;
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
  value_scope.get_with_host_and_hooks(vm, host, hooks, obj, value_key, iter_result)
}

/// `IteratorStep` (ECMA-262).
///
/// Returns `Ok(None)` when iteration is complete.
pub fn iterator_step(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &mut IteratorRecord,
) -> Result<Option<Value>, VmError> {
  if record.done {
    return Ok(None);
  }

  let result = iterator_next(vm, host, hooks, scope, record, None)?;

  // Spec: if `IteratorComplete` throws, set `[[Done]] = true` so callers skip IteratorClose.
  let done = match iterator_complete(vm, host, hooks, scope, result) {
    Ok(v) => v,
    Err(err) => {
      record.done = true;
      return Err(err);
    }
  };
  if done {
    record.done = true;
    return Ok(None);
  }

  Ok(Some(result))
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

  let Some(result) = iterator_step(vm, host, hooks, scope, record)? else {
    return Ok(None);
  };

  match iterator_value(vm, host, hooks, scope, result) {
    Ok(v) => Ok(Some(v)),
    Err(err) => {
      record.done = true;
      Err(err)
    }
  }
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
  completion_kind: CloseCompletionKind,
) -> Result<(), VmError> {
  // Root the iterator across property-key allocation and the `GetMethod`/`Call` sequence. Without
  // this, the iterator object could be collected while closing (since values on the Rust stack are
  // not traced by the GC).
  let mut close_scope = scope.reborrow();
  close_scope.push_root(record.iterator)?;

  let return_key = string_key(&mut close_scope, "return")?;
  let return_method = match crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut close_scope,
    host,
    hooks,
    record.iterator,
    return_key,
  ) {
    Ok(m) => m,
    Err(err) => {
      if completion_kind == CloseCompletionKind::Throw && err.is_throw_completion() {
        return Ok(());
      }
      return Err(err);
    }
  };

  let Some(return_method) = return_method else {
    return Ok(());
  };

  close_scope.push_root(return_method)?;
  let result = match vm.call_with_host_and_hooks(
    host,
    &mut close_scope,
    hooks,
    return_method,
    record.iterator,
    &[],
  ) {
    Ok(v) => v,
    Err(err) => {
      if completion_kind == CloseCompletionKind::Throw && err.is_throw_completion() {
        return Ok(());
      }
      return Err(err);
    }
  };

  if completion_kind == CloseCompletionKind::Throw {
    // Spec: for throw completions, ignore non-object `return` results.
    return Ok(());
  }

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

/// `IteratorClose` (ECMA-262) with completion-sensitive error precedence.
///
/// This is a convenience wrapper for callers that need the *full* `IteratorClose` semantics from
/// ECMA-262 (which takes an input completion):
/// - Always attempts `GetMethod(iterator, "return")` and calls it when present.
/// - If `completion_is_throw` is `true`, any *throw completion* produced by `GetMethod`/`Call` (or
///   by the "return result is not object" check) is suppressed.
/// - If `completion_is_throw` is `false`, closing errors are propagated (and thus override
///   non-throw completions like `break`/`return`).
pub fn iterator_close_strict(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &IteratorRecord,
  completion_is_throw: bool,
) -> Result<(), VmError> {
  let completion_kind = if completion_is_throw {
    CloseCompletionKind::Throw
  } else {
    CloseCompletionKind::NonThrow
  };
  iterator_close(vm, host, hooks, scope, record, completion_kind)
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

  // `closeIterator` implements `IteratorClose(syncIteratorRecord, ThrowCompletion(reason))`.
  // Closing errors are suppressed for throw completions, but fatal VM errors (OOM/termination) must
  // still propagate.
  iterator_close(vm, host, hooks, &mut scope, &record, CloseCompletionKind::Throw)?;
  Err(VmError::Throw(reason))
}

/// ECMAScript async Iterator Record (ECMA-262).
#[derive(Debug, Clone, Copy)]
pub struct AsyncIteratorRecord {
  pub iterator: Value,
  pub next_method: Value,
  pub done: bool,
}

fn promise_reject(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  reason: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(reason)?;

  let cap =
    crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;

  // Root the resolving functions and reason across the reject call in case it allocates.
  scope.push_roots(&[cap.promise, cap.reject, reason])?;

  let _ = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    cap.reject,
    Value::Undefined,
    &[reason],
  )?;
  Ok(cap.promise)
}

fn reject_promise_from_vm_error(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  err: VmError,
) -> Result<Value, VmError> {
  let Some(reason) = err.thrown_value() else {
    return Err(err);
  };
  promise_reject(vm, host, hooks, scope, reason)
}

fn promise_resolve_undefined(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
) -> Result<Value, VmError> {
  crate::promise_ops::promise_resolve_with_host_and_hooks(vm, scope, host, hooks, Value::Undefined)
}

fn async_from_sync_iterator_continuation(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  sync_iterator: Value,
  result: Value,
) -> Result<Value, VmError> {
  // Root the sync iterator + iterator result across `IteratorComplete`/`IteratorValue`, which can
  // allocate (string keys) and trigger GC.
  let mut scope = scope.reborrow();
  scope.push_roots(&[sync_iterator, result])?;

  let done = iterator_complete(vm, host, hooks, &mut scope, result)?;
  let value = iterator_value(vm, host, hooks, &mut scope, result)?;
  scope.push_root(value)?;

  let value_wrapper = match crate::promise_ops::promise_resolve_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    value,
  ) {
    Ok(p) => p,
    Err(err) => {
      if !done {
        let original_is_throw = err.is_throw_completion();
        // Root the thrown value across `IteratorClose`, which can allocate and trigger GC.
        if original_is_throw {
          if let Some(thrown) = err.thrown_value() {
            scope.push_root(thrown)?;
          }
        }

        let record = IteratorRecord {
          iterator: sync_iterator,
          next_method: Value::Undefined,
          done: false,
        };

        // `AsyncFromSyncIteratorContinuation` calls `IteratorClose(syncIteratorRecord, valueWrapper)`
        // where `valueWrapper` is a throw completion (`PromiseResolve` failed), so close errors are
        // suppressed unless they represent a fatal VM failure (OOM/termination/etc).
        if let Err(close_err) =
          iterator_close(vm, host, hooks, &mut scope, &record, CloseCompletionKind::Throw)
        {
          if original_is_throw && !close_err.is_throw_completion() {
            return Err(close_err);
          }
        }
      }
      return Err(err);
    }
  };
  scope.push_root(value_wrapper)?;

  let unwrap_call_id = vm.async_from_sync_iterator_unwrap_call_id()?;
  let unwrap_name = scope.alloc_string("")?;
  let unwrap = scope.alloc_native_function_with_slots(
    unwrap_call_id,
    None,
    unwrap_name,
    1,
    &[Value::Bool(done)],
  )?;
  // Root the unwrap handler before allocating the optional close handler (and before calling
  // `PerformPromiseThen`), since Rust stack locals are not traced by the GC.
  scope.push_root(Value::Object(unwrap))?;

  let on_rejected = if done {
    None
  } else {
    let close_call_id = vm.async_from_sync_iterator_close_call_id()?;
    let close_name = scope.alloc_string("")?;
    let close = scope.alloc_native_function_with_slots(
      close_call_id,
      None,
      close_name,
      1,
      &[sync_iterator],
    )?;
    scope.push_root(Value::Object(close))?;
    Some(Value::Object(close))
  };

  crate::promise_ops::perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    value_wrapper,
    Some(Value::Object(unwrap)),
    on_rejected,
  )
}

fn create_async_from_sync_iterator(
  vm: &mut Vm,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  sync_record: IteratorRecord,
) -> Result<AsyncIteratorRecord, VmError> {
  let mut scope = scope.reborrow();
  scope.push_roots(&[sync_record.iterator, sync_record.next_method])?;

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;
  if let Some(intr) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(wrapper, Some(intr.object_prototype()))?;
  }

  let next_call_id = vm.async_from_sync_iterator_next_call_id()?;
  let return_call_id = vm.async_from_sync_iterator_return_call_id()?;
  let throw_call_id = vm.async_from_sync_iterator_throw_call_id()?;

  let slots = [sync_record.iterator, sync_record.next_method];

  let next_name = scope.alloc_string("next")?;
  let next_fn = scope.alloc_native_function_with_slots(
    next_call_id,
    None,
    next_name,
    1,
    &slots,
  )?;
  // Root each method function while allocating the rest and while defining properties on the
  // wrapper.
  scope.push_root(Value::Object(next_fn))?;
  let return_name = scope.alloc_string("return")?;
  let return_fn = scope.alloc_native_function_with_slots(
    return_call_id,
    None,
    return_name,
    1,
    &slots,
  )?;
  scope.push_root(Value::Object(return_fn))?;
  let throw_name = scope.alloc_string("throw")?;
  let throw_fn = scope.alloc_native_function_with_slots(
    throw_call_id,
    None,
    throw_name,
    1,
    &slots,
  )?;
  scope.push_root(Value::Object(throw_fn))?;

  let next_key = string_key(&mut scope, "next")?;
  crate::spec_ops::create_data_property_or_throw(
    &mut scope,
    wrapper,
    next_key,
    Value::Object(next_fn),
  )?;
  let return_key = string_key(&mut scope, "return")?;
  crate::spec_ops::create_data_property_or_throw(
    &mut scope,
    wrapper,
    return_key,
    Value::Object(return_fn),
  )?;
  let throw_key = string_key(&mut scope, "throw")?;
  crate::spec_ops::create_data_property_or_throw(
    &mut scope,
    wrapper,
    throw_key,
    Value::Object(throw_fn),
  )?;

  Ok(AsyncIteratorRecord {
    iterator: Value::Object(wrapper),
    next_method: Value::Object(next_fn),
    done: false,
  })
}

/// `GetAsyncIterator` (ECMA-262).
pub fn get_async_iterator(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
) -> Result<AsyncIteratorRecord, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let async_iter_sym = intr.well_known_symbols().async_iterator;
  let iter_sym = intr.well_known_symbols().iterator;

  if let Some(method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    iterable,
    PropertyKey::from_symbol(async_iter_sym),
  )? {
    return get_async_iterator_from_method(vm, host, hooks, scope, iterable, method);
  }

  let Some(sync_method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    iterable,
    PropertyKey::from_symbol(iter_sym),
  )?
  else {
    return Err(VmError::TypeError("GetAsyncIterator: value is not async iterable"));
  };

  let sync_record = get_iterator_from_method(vm, host, hooks, scope, iterable, sync_method)?;
  create_async_from_sync_iterator(vm, host, hooks, scope, sync_record)
}

fn get_async_iterator_from_method(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterable: Value,
  method: Value,
) -> Result<AsyncIteratorRecord, VmError> {
  let iterator = vm.call_with_host_and_hooks(host, scope, hooks, method, iterable, &[])?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(VmError::TypeError(
      "GetAsyncIterator: iterator method did not return an object",
    ));
  };

  let mut next_scope = scope.reborrow();
  next_scope.push_root(iterator)?;

  let next_key = string_key(&mut next_scope, "next")?;
  let next = next_scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    iterator_obj,
    next_key,
    Value::Object(iterator_obj),
  )?;
  if !next_scope.heap().is_callable(next)? {
    return Err(VmError::TypeError("GetAsyncIterator: iterator.next is not callable"));
  }

  Ok(AsyncIteratorRecord {
    iterator,
    next_method: next,
    done: false,
  })
}

/// `AsyncIteratorNext` (ECMA-262) (partial).
pub fn async_iterator_next(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &AsyncIteratorRecord,
) -> Result<Value, VmError> {
  vm.call_with_host_and_hooks(host, scope, hooks, record.next_method, record.iterator, &[])
}

pub(crate) fn async_from_sync_iterator_next_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "AsyncFromSyncIterator.next missing native slots",
    ));
  }
  let sync_iterator = slots[0];
  let sync_next = slots[1];

  let next_args: [Value; 1];
  let args_slice = if let Some(v) = args.get(0).copied() {
    next_args = [v];
    next_args.as_slice()
  } else {
    &[][..]
  };

  let mut scope = scope.reborrow();
  scope.push_roots(&[sync_iterator, sync_next])?;

  let result = match vm.call_with_host_and_hooks(host, &mut scope, hooks, sync_next, sync_iterator, args_slice) {
    Ok(v) => v,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
    let reason =
      crate::error_object::new_type_error_object(&mut scope, &intr, "IteratorNext returned non-object")?;
    return promise_reject(vm, host, hooks, &mut scope, reason);
  }

  match async_from_sync_iterator_continuation(vm, host, hooks, &mut scope, sync_iterator, result) {
    Ok(promise) => Ok(promise),
    Err(err) => reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  }
}

pub(crate) fn async_from_sync_iterator_return_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "AsyncFromSyncIterator.return missing native slots",
    ));
  }
  let sync_iterator = slots[0];

  let mut scope = scope.reborrow();
  scope.push_root(sync_iterator)?;

  let return_key = string_key(&mut scope, "return")?;
  let return_method = match crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    sync_iterator,
    return_key,
  ) {
    Ok(m) => m,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(value)?;

  let Some(return_method) = return_method else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
    let iter_result = {
      let mut obj_scope = scope.reborrow();
      obj_scope.push_root(value)?;
      let out = obj_scope.alloc_object()?;
      obj_scope.push_root(Value::Object(out))?;
      obj_scope
        .heap_mut()
        .object_set_prototype(out, Some(intr.object_prototype()))?;
      let value_key = string_key(&mut obj_scope, "value")?;
      let done_key = string_key(&mut obj_scope, "done")?;
      crate::spec_ops::create_data_property_or_throw(&mut obj_scope, out, value_key, value)?;
      crate::spec_ops::create_data_property_or_throw(&mut obj_scope, out, done_key, Value::Bool(true))?;
      Value::Object(out)
    };
    return crate::promise_ops::promise_resolve_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      iter_result,
    );
  };

  let call_args: [Value; 1];
  let args_slice = if args.get(0).is_some() {
    call_args = [value];
    call_args.as_slice()
  } else {
    &[][..]
  };

  let result = match vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    return_method,
    sync_iterator,
    args_slice,
  ) {
    Ok(v) => v,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "Iterator return returned non-object",
    )?;
    return promise_reject(vm, host, hooks, &mut scope, reason);
  }

  match async_from_sync_iterator_continuation(vm, host, hooks, &mut scope, sync_iterator, result) {
    Ok(promise) => Ok(promise),
    Err(err) => reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  }
}

pub(crate) fn async_from_sync_iterator_throw_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  if slots.len() != 2 {
    return Err(VmError::InvariantViolation(
      "AsyncFromSyncIterator.throw missing native slots",
    ));
  }
  let sync_iterator = slots[0];

  let mut scope = scope.reborrow();
  scope.push_root(sync_iterator)?;

  let throw_key = string_key(&mut scope, "throw")?;
  let throw_method = match crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    sync_iterator,
    throw_key,
  ) {
    Ok(m) => m,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(value)?;

  let Some(throw_method) = throw_method else {
    // Close the sync iterator, then reject with TypeError.
    let record = IteratorRecord {
      iterator: sync_iterator,
      next_method: Value::Undefined,
      done: false,
    };
    if let Err(err) = iterator_close(vm, host, hooks, &mut scope, &record, CloseCompletionKind::Throw) {
      return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err);
    }
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "Iterator does not have a throw method",
    )?;
    return promise_reject(vm, host, hooks, &mut scope, reason);
  };

  let call_args: [Value; 1];
  let args_slice = if args.get(0).is_some() {
    call_args = [value];
    call_args.as_slice()
  } else {
    &[][..]
  };

  let result = match vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    throw_method,
    sync_iterator,
    args_slice,
  ) {
    Ok(v) => v,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
    let reason =
      crate::error_object::new_type_error_object(&mut scope, &intr, "Iterator throw returned non-object")?;
    return promise_reject(vm, host, hooks, &mut scope, reason);
  }

  match async_from_sync_iterator_continuation(vm, host, hooks, &mut scope, sync_iterator, result) {
    Ok(promise) => Ok(promise),
    Err(err) => reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  }
}

/// `AsyncIteratorClose` (ECMA-262) (partial).
///
/// Returns a Promise that fulfills when the iterator is closed, and rejects with any error thrown
/// by `iterator.return()` or awaiting/validating its result.
pub fn async_iterator_close(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &AsyncIteratorRecord,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(record.iterator)?;

  let return_key = string_key(&mut scope, "return")?;
  let return_method = match crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    record.iterator,
    return_key,
  ) {
    Ok(m) => m,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };
  let Some(return_method) = return_method else {
    return promise_resolve_undefined(vm, host, hooks, &mut scope);
  };

  scope.push_root(return_method)?;
  let return_result = match vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    return_method,
    record.iterator,
    &[],
  ) {
    Ok(v) => v,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };

  let awaited = match crate::promise_ops::promise_resolve_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    return_result,
  ) {
    Ok(v) => v,
    Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut scope, err),
  };
  scope.push_root(awaited)?;

  let on_fulfilled_call_id = vm.async_iterator_close_on_fulfilled_call_id()?;
  let on_rejected_call_id = vm.async_iterator_close_on_rejected_call_id()?;
  let on_fulfilled_name = scope.alloc_string("")?;
  let on_fulfilled = scope.alloc_native_function(on_fulfilled_call_id, None, on_fulfilled_name, 1)?;
  scope.push_root(Value::Object(on_fulfilled))?;
  let on_rejected_name = scope.alloc_string("")?;
  let on_rejected = scope.alloc_native_function(on_rejected_call_id, None, on_rejected_name, 1)?;
  scope.push_root(Value::Object(on_rejected))?;

  crate::promise_ops::perform_promise_then_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    awaited,
    Some(Value::Object(on_fulfilled)),
    Some(Value::Object(on_rejected)),
  )
}

pub(crate) fn async_iterator_close_on_fulfilled_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(v, Value::Object(_)) {
    return Err(VmError::TypeError(
      "AsyncIteratorClose: return result is not an object",
    ));
  }
  Ok(Value::Undefined)
}

pub(crate) fn async_iterator_close_on_rejected_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  Err(VmError::Throw(reason))
}
