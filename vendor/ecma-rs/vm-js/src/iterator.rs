use crate::property::PropertyKey;
use crate::{
  Completion, GcObject, PromiseCapability, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

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
  /// Closing on a *throw* completion.
  ///
  /// Per ECMA-262 `IteratorClose(iteratorRecord, completion)`, `GetMethod(iterator, "return")` and
  /// `Call(return, iterator)` are still performed when possible. However, if the incoming
  /// completion is itself a throw completion, any error thrown while **calling**
  /// `iterator.return` is **suppressed** in favour of the original throw completion.
  ///
  /// Errors thrown while **getting** `iterator.return` (including observable getter side effects)
  /// are still propagated and therefore override the incoming completion (since the spec's
  /// `completion.[[Type]] is throw` check happens *after* `GetMethod` / `Call`).
  ///
  /// This is observable in user code because the `return` getter is still invoked, but its thrown
  /// value does not replace the original exception when the getter completes normally and the
  /// *call* to `return` throws.
  ///
  /// The non-object return-result TypeError check is also skipped for throw completions (because
  /// the incoming throw completion is returned before that check is performed).
  ///
  /// Note: `vm-js` has non-catchable VM failures (OOM/termination/etc). Those are still propagated
  /// even when closing on a throw completion.
  Throw,
  /// Closing on a *non-throw* completion.
  ///
  /// Per ECMA-262 `IteratorClose`, errors thrown while getting/calling `iterator.return` override
  /// the incoming completion, and a non-object return value from `iterator.return` throws a
  /// TypeError.
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
  // Root the inputs across the call to the iterator method: the call can allocate/GC, and the
  // `method`/`iterable` values may not be reachable from any heap object (e.g. when
  // `GetIteratorFromMethod` is invoked directly with a native function in tests).
  let mut scope = scope.reborrow();
  scope.push_roots(&[iterable, method])?;

  let iterator = vm.call_with_host_and_hooks(host, &mut scope, hooks, method, iterable, &[])?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(VmError::TypeError(
      "GetIteratorFromMethod: iterator method did not return an object",
    ));
  };

  // Root the iterator object while allocating/reading the `next` method in case those operations
  // trigger GC.
  scope.push_root(iterator)?;

  let next_key = string_key(&mut scope, "next")?;
  let next = scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    iterator_obj,
    next_key,
    Value::Object(iterator_obj),
  )?;

  // `GetIteratorFromMethod` must return an iterator record with a callable `next` method.
  if !scope.heap().is_callable(next)? {
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

  let result = match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    record.next_method,
    record.iterator,
    args,
  ) {
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

  // Spec: if `IteratorComplete` throws, set `[[Done]] = true` before propagating the error so
  // callers skip `IteratorClose` after an iterator protocol error.
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
      // Spec: if `IteratorValue` throws, set `[[Done]] = true` before propagating the error so
      // callers skip `IteratorClose` after an iterator protocol error.
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
  if record.done {
    return Ok(());
  }

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
      // ECMA-262 `IteratorClose`: if the incoming completion is a throw completion, return it
      // before propagating errors thrown while getting `iterator.return`.
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
      // ECMA-262 `IteratorClose`: if the incoming completion is a throw completion, return it
      // before propagating errors thrown while calling `iterator.return()`.
      if completion_kind == CloseCompletionKind::Throw && err.is_throw_completion() {
        return Ok(());
      }
      return Err(err);
    }
  };

  if completion_kind == CloseCompletionKind::Throw {
    // Spec: for throw completions, return the incoming completion before performing the
    // return-result type check (so non-object return values are ignored).
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

/// `IteratorClose` (ECMA-262) with completion-sensitive return-result checks.
///
/// This is a convenience wrapper for callers that need completion-sensitive `IteratorClose`
/// semantics but do not want to thread an explicit [`Completion`] value:
/// - Always attempts `GetMethod(iterator, "return")` and calls it when present.
/// - If `completion_is_throw` is `true`, any JavaScript exceptions thrown while getting/calling
///   `iterator.return` are ignored (the incoming throw completion is preserved).
/// - If `completion_is_throw` is `false`, errors thrown while getting/calling `iterator.return`
///   override the completion.
/// - If `completion_is_throw` is `true`, the return-result type check is skipped.
/// - If `completion_is_throw` is `false`, a non-object return result throws a TypeError.
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

/// `IteratorClose(iteratorRecord, completion)` (ECMA-262).
///
/// This is the spec-shaped form of iterator closing used by `for..of` and iterator-consuming
/// algorithms:
/// - Always attempts `GetMethod(iterator, "return")` and calls it when present.
/// - If `completion` is a throw completion, JavaScript exceptions thrown while getting/calling
///   `iterator.return` are ignored and `completion` is returned.
/// - If `completion` is not a throw completion, errors thrown while getting/calling
///   `iterator.return` override `completion` and are returned.
/// - If `completion` is a throw completion, the non-object return-result TypeError check is
///   skipped.
/// - If `completion` is a non-throw completion, a non-object return result throws a TypeError.
pub fn iterator_close_with_completion(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &IteratorRecord,
  completion: Completion,
) -> Result<Completion, VmError> {
  if record.done {
    return Ok(completion);
  }

  let completion_is_throw = matches!(&completion, Completion::Throw(_));

  // Root the completion value (if any) across IteratorClose, since it can allocate and run user
  // code.
  let mut close_scope = scope.reborrow();
  if let Some(v) = completion.value() {
    close_scope.push_root(v)?;
  }

  iterator_close(
    vm,
    host,
    hooks,
    &mut close_scope,
    record,
    if completion_is_throw {
      CloseCompletionKind::Throw
    } else {
      CloseCompletionKind::NonThrow
    },
  )?;

  Ok(completion)
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
    _ => {
      return Err(VmError::InvariantViolation(
        "AsyncFromSyncIterator unwrap missing done slot",
      ))
    }
  };
  let v = args.get(0).copied().unwrap_or(Value::Undefined);

  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "AsyncFromSyncIterator requires intrinsics",
  ))?;

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
  // Per ECMA-262 `IteratorClose`, errors thrown while getting/calling `iterator.return` are ignored
  // when closing on a throw completion. Only the non-object return-result TypeError check is
  // skipped for throw completions (since the incoming throw completion is returned before the
  // return-result type check is performed).
  iterator_close(
    vm,
    host,
    hooks,
    &mut scope,
    &record,
    CloseCompletionKind::Throw,
  )?;
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

fn reject_promise_from_throw_completion_error(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  err: VmError,
) -> Result<Value, VmError> {
  let reason = vm_error_to_rejection_reason(vm, scope, err)?;
  promise_reject(vm, host, hooks, scope, reason)
}

fn vm_error_to_rejection_reason(
  vm: &Vm,
  scope: &mut Scope<'_>,
  err: VmError,
) -> Result<Value, VmError> {
  if let Some(reason) = err.thrown_value() {
    return Ok(reason);
  }

  // `AsyncFromSyncIterator` methods must reject their result promise with the same value that would
  // have been thrown, even when the error is represented as an internal helper variant (TypeError,
  // NotCallable, etc).
  if !err.is_throw_completion() {
    return Err(err);
  }

  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "AsyncFromSyncIterator requires intrinsics",
  ))?;

  match err {
    VmError::TypeError(message) => {
      crate::error_object::new_type_error_object(scope, &intr, message)
    }
    VmError::RangeError(message) => crate::error_object::new_range_error(scope, intr, message),
    VmError::NotCallable => {
      crate::error_object::new_type_error_object(scope, &intr, "value is not callable")
    }
    VmError::NotConstructable => {
      crate::error_object::new_type_error_object(scope, &intr, "value is not a constructor")
    }
    VmError::PrototypeCycle => {
      crate::error_object::new_type_error_object(scope, &intr, "prototype cycle")
    }
    VmError::PrototypeChainTooDeep => {
      crate::error_object::new_type_error_object(scope, &intr, "prototype chain too deep")
    }
    VmError::InvalidPropertyDescriptorPatch => crate::error_object::new_type_error_object(
      scope,
      &intr,
      "invalid property descriptor patch: cannot mix data and accessor fields",
    ),
    // `Throw`/`ThrowWithStack` would have been handled by `thrown_value()` above; anything else that
    // claims to be a throw completion should be treated as a non-rejectable fatal error.
    _ => Err(err),
  }
}

fn reject_promise_with_capability(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  capability: PromiseCapability,
  reason: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_roots(&[capability.promise, capability.reject, reason])?;
  let _ = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    capability.reject,
    Value::Undefined,
    &[reason],
  )?;
  Ok(capability.promise)
}

fn resolve_promise_with_capability(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  capability: PromiseCapability,
  value: Value,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_roots(&[capability.promise, capability.resolve, value])?;
  let _ = vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    capability.resolve,
    Value::Undefined,
    &[value],
  )?;
  Ok(capability.promise)
}

fn if_abrupt_reject_promise(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  capability: PromiseCapability,
  err: VmError,
) -> Result<Value, VmError> {
  let reason = vm_error_to_rejection_reason(vm, scope, err)?;
  reject_promise_with_capability(vm, host, hooks, scope, capability, reason)
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
  promise_capability: PromiseCapability,
  close_on_rejection: bool,
) -> Result<Value, VmError> {
  // Root the sync iterator + iterator result across `IteratorComplete`/`IteratorValue`, which can
  // allocate (string keys) and trigger GC.
  let mut scope = scope.reborrow();
  scope.push_roots(&[sync_iterator, result])?;

  let done = match iterator_complete(vm, host, hooks, &mut scope, result) {
    Ok(v) => v,
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };
  let value = match iterator_value(vm, host, hooks, &mut scope, result) {
    Ok(v) => v,
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };
  scope.push_root(value)?;

  let value_wrapper = match crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
    vm, &mut scope, host, hooks, value,
  ) {
    Ok(p) => p,
    Err(err) => {
      if !done && close_on_rejection {
        // Root the thrown value across `IteratorClose`, which can allocate and trigger GC.
        let original_is_throw = err.is_throw_completion();
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
        // where `valueWrapper` is a throw completion (`PromiseResolve` failed). Per ECMA-262
        // `IteratorClose`, throw completions suppress errors thrown while getting/calling
        // `iterator.return` (the original `PromiseResolve` throw is preserved). However, fatal VM
        // failures (OOM/termination/etc) are still propagated.
        if let Err(close_err) = iterator_close(
          vm,
          host,
          hooks,
          &mut scope,
          &record,
          CloseCompletionKind::Throw,
        ) {
          if original_is_throw {
            return if_abrupt_reject_promise(
              vm,
              host,
              hooks,
              &mut scope,
              promise_capability,
              close_err,
            );
          }
        }
      }
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err);
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

  let on_rejected = if done || !close_on_rejection {
    Value::Undefined
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
    Value::Object(close)
  };

  let promise =
    crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      value_wrapper,
      Value::Object(unwrap),
      on_rejected,
      Some(promise_capability),
    )?
    .ok_or(VmError::InvariantViolation(
      "PerformPromiseThen with capability returned undefined",
    ))?;
  Ok(promise)
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
      .object_set_prototype(wrapper, Some(intr.async_iterator_prototype()))?;
  }

  let next_call_id = vm.async_from_sync_iterator_next_call_id()?;
  let return_call_id = vm.async_from_sync_iterator_return_call_id()?;
  let throw_call_id = vm.async_from_sync_iterator_throw_call_id()?;

  let slots = [sync_record.iterator, sync_record.next_method];

  let next_name = scope.alloc_string("next")?;
  let next_fn = scope.alloc_native_function_with_slots(next_call_id, None, next_name, 1, &slots)?;
  // Root each method function while allocating the rest and while defining properties on the
  // wrapper.
  scope.push_root(Value::Object(next_fn))?;
  let return_name = scope.alloc_string("return")?;
  let return_fn =
    scope.alloc_native_function_with_slots(return_call_id, None, return_name, 1, &slots)?;
  scope.push_root(Value::Object(return_fn))?;
  let throw_name = scope.alloc_string("throw")?;
  let throw_fn =
    scope.alloc_native_function_with_slots(throw_call_id, None, throw_name, 1, &slots)?;
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
    return Err(VmError::TypeError(
      "GetAsyncIterator: value is not async iterable",
    ));
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
  // Root `iterable`/`method` across the call for the same reason as `GetIteratorFromMethod`.
  let mut scope = scope.reborrow();
  scope.push_roots(&[iterable, method])?;
  let iterator = vm.call_with_host_and_hooks(host, &mut scope, hooks, method, iterable, &[])?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(VmError::TypeError(
      "GetAsyncIterator: iterator method did not return an object",
    ));
  };

  scope.push_root(iterator)?;

  let next_key = string_key(&mut scope, "next")?;
  let next = scope.get_with_host_and_hooks(
    vm,
    host,
    hooks,
    iterator_obj,
    next_key,
    Value::Object(iterator_obj),
  )?;
  if !scope.heap().is_callable(next)? {
    return Err(VmError::TypeError(
      "GetAsyncIterator: iterator.next is not callable",
    ));
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

  let promise_capability =
    crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;
  scope.push_roots(&[
    promise_capability.promise,
    promise_capability.resolve,
    promise_capability.reject,
  ])?;

  let result = match vm.call_with_host_and_hooks(
    host,
    &mut scope,
    hooks,
    sync_next,
    sync_iterator,
    args_slice,
  ) {
    Ok(v) => v,
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "AsyncFromSyncIterator requires intrinsics",
    ))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "IteratorNext returned non-object",
    )?;
    return reject_promise_with_capability(vm, host, hooks, &mut scope, promise_capability, reason);
  }

  async_from_sync_iterator_continuation(
    vm,
    host,
    hooks,
    &mut scope,
    sync_iterator,
    result,
    promise_capability,
    true,
  )
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

  let promise_capability =
    crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;
  scope.push_roots(&[
    promise_capability.promise,
    promise_capability.resolve,
    promise_capability.reject,
  ])?;

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
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  scope.push_root(value)?;

  let Some(return_method) = return_method else {
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "AsyncFromSyncIterator requires intrinsics",
    ))?;
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
      crate::spec_ops::create_data_property_or_throw(
        &mut obj_scope,
        out,
        done_key,
        Value::Bool(true),
      )?;
      Value::Object(out)
    };
    return resolve_promise_with_capability(
      vm,
      host,
      hooks,
      &mut scope,
      promise_capability,
      iter_result,
    );
  };
  scope.push_root(return_method)?;

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
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "AsyncFromSyncIterator requires intrinsics",
    ))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "Iterator return returned non-object",
    )?;
    return reject_promise_with_capability(vm, host, hooks, &mut scope, promise_capability, reason);
  }

  async_from_sync_iterator_continuation(
    vm,
    host,
    hooks,
    &mut scope,
    sync_iterator,
    result,
    promise_capability,
    false,
  )
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

  let promise_capability =
    crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;
  scope.push_roots(&[
    promise_capability.promise,
    promise_capability.resolve,
    promise_capability.reject,
  ])?;

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
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
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
    if let Err(err) = iterator_close(
      vm,
      host,
      hooks,
      &mut scope,
      &record,
      CloseCompletionKind::Throw,
    ) {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err);
    }
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "AsyncFromSyncIterator requires intrinsics",
    ))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "Iterator does not have a throw method",
    )?;
    return reject_promise_with_capability(vm, host, hooks, &mut scope, promise_capability, reason);
  };
  scope.push_root(throw_method)?;

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
    Err(err) => {
      return if_abrupt_reject_promise(vm, host, hooks, &mut scope, promise_capability, err)
    }
  };
  if !matches!(result, Value::Object(_)) {
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "AsyncFromSyncIterator requires intrinsics",
    ))?;
    let reason = crate::error_object::new_type_error_object(
      &mut scope,
      &intr,
      "Iterator throw returned non-object",
    )?;
    return reject_promise_with_capability(vm, host, hooks, &mut scope, promise_capability, reason);
  }

  async_from_sync_iterator_continuation(
    vm,
    host,
    hooks,
    &mut scope,
    sync_iterator,
    result,
    promise_capability,
    true,
  )
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
  async_iterator_close_with_completion_kind(
    vm,
    host,
    hooks,
    scope,
    record,
    CloseCompletionKind::NonThrow,
  )
}

/// `AsyncIteratorClose(iteratorRecord, completion)` (ECMA-262) (partial).
///
/// This is the completion-sensitive form of [`async_iterator_close`]. It implements the key
/// suppression semantics of ECMA-262 `AsyncIteratorClose`:
/// - Always attempts `GetMethod(iterator, "return")` and calls it when present.
/// - If `completion_kind` is [`CloseCompletionKind::Throw`], any JavaScript exception thrown while
///   getting/calling/awaiting `iterator.return` is suppressed (the incoming throw completion "wins")
///   and the return-result type check is skipped.
/// - If `completion_kind` is [`CloseCompletionKind::NonThrow`], closing errors reject/throw as
///   normal and a non-object return result produces a TypeError.
/// - Fatal VM errors (OOM/termination/etc) are never suppressed.
///
/// Returns a Promise that fulfills when the close operation completes (including waiting for the
/// `iterator.return()` result to settle when present), or rejects with the close error when it is
/// not suppressed.
pub fn async_iterator_close_with_completion_kind(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &AsyncIteratorRecord,
  completion_kind: CloseCompletionKind,
) -> Result<Value, VmError> {
  if record.done {
    return promise_resolve_undefined(vm, host, hooks, scope);
  }

  let suppress_throw = completion_kind == CloseCompletionKind::Throw;

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
    Err(err) => {
      if suppress_throw && err.is_throw_completion() {
        return promise_resolve_undefined(vm, host, hooks, &mut scope);
      }
      return reject_promise_from_throw_completion_error(vm, host, hooks, &mut scope, err);
    }
  };

  let Some(return_method) = return_method else {
    return promise_resolve_undefined(vm, host, hooks, &mut scope);
  };

  scope.push_root(return_method)?;
  let return_result =
    match vm.call_with_host_and_hooks(host, &mut scope, hooks, return_method, record.iterator, &[])
    {
      Ok(v) => v,
      Err(err) => {
        if suppress_throw && err.is_throw_completion() {
          return promise_resolve_undefined(vm, host, hooks, &mut scope);
        }
        return reject_promise_from_throw_completion_error(vm, host, hooks, &mut scope, err);
      }
    };

  let awaited = match crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    return_result,
  ) {
    Ok(v) => v,
    Err(err) => {
      if suppress_throw && err.is_throw_completion() {
        return promise_resolve_undefined(vm, host, hooks, &mut scope);
      }
      return reject_promise_from_throw_completion_error(vm, host, hooks, &mut scope, err);
    }
  };
  scope.push_root(awaited)?;

  let on_fulfilled_call_id = vm.async_iterator_close_on_fulfilled_call_id()?;
  let on_rejected_call_id = vm.async_iterator_close_on_rejected_call_id()?;

  // Slot 0 for both handlers:
  // - onFulfilled: `check_object` (default true)
  // - onRejected: `suppress` (default false)
  let check_object = Value::Bool(!suppress_throw);
  let suppress_rejection = Value::Bool(suppress_throw);

  let on_fulfilled_name = scope.alloc_string("")?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    on_fulfilled_call_id,
    None,
    on_fulfilled_name,
    1,
    &[check_object],
  )?;
  scope.push_root(Value::Object(on_fulfilled))?;

  let on_rejected_name = scope.alloc_string("")?;
  let on_rejected = scope.alloc_native_function_with_slots(
    on_rejected_call_id,
    None,
    on_rejected_name,
    1,
    &[suppress_rejection],
  )?;
  scope.push_root(Value::Object(on_rejected))?;

  let promise_capability =
    crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;
  scope.push_roots(&[
    promise_capability.promise,
    promise_capability.resolve,
    promise_capability.reject,
  ])?;

  let promise =
    crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      awaited,
      Value::Object(on_fulfilled),
      Value::Object(on_rejected),
      Some(promise_capability),
    )?
    .ok_or(VmError::InvariantViolation(
      "PerformPromiseThen with capability returned undefined",
    ))?;
  Ok(promise)
}

pub(crate) fn async_iterator_close_on_fulfilled_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let check_object = match scope
    .heap()
    .get_function_native_slots(callee)?
    .get(0)
    .copied()
  {
    None => true,
    Some(Value::Bool(b)) => b,
    Some(_) => {
      return Err(VmError::InvariantViolation(
        "AsyncIteratorClose onFulfilled handler expected boolean slot 0",
      ));
    }
  };

  if !check_object {
    return Ok(Value::Undefined);
  }

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
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let suppress = match scope
    .heap()
    .get_function_native_slots(callee)?
    .get(0)
    .copied()
  {
    None => false,
    Some(Value::Bool(b)) => b,
    Some(_) => {
      return Err(VmError::InvariantViolation(
        "AsyncIteratorClose onRejected handler expected boolean slot 0",
      ));
    }
  };
  if suppress {
    return Ok(Value::Undefined);
  }
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  Err(VmError::Throw(reason))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::property::{PropertyDescriptor, PropertyKind};
  use crate::{Heap, HeapLimits, MicrotaskQueue, PromiseState, Realm, VmOptions};

  #[derive(Default)]
  struct TestHost {
    return_calls: usize,
  }

  fn method_returns_slot0(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let Some(v) = slots.get(0).copied() else {
      return Err(VmError::InvariantViolation("expected native slot 0"));
    };
    Ok(v)
  }

  fn return_increments_host_and_returns_slot0(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Some(host) = host.as_any_mut().downcast_mut::<TestHost>() else {
      return Err(VmError::InvariantViolation("expected TestHost"));
    };
    host.return_calls += 1;
    method_returns_slot0(_vm, scope, host, _hooks, callee, _this, _args)
  }

  fn throw_slot0(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let Some(v) = slots.get(0).copied() else {
      return Err(VmError::InvariantViolation("expected native slot 0"));
    };
    Err(VmError::Throw(v))
  }

  fn value_to_utf8_lossy(heap: &Heap, value: Value) -> Result<String, VmError> {
    let Value::String(s) = value else {
      return Err(VmError::InvariantViolation("expected string"));
    };
    Ok(heap.get_string(s)?.to_utf8_lossy())
  }

  fn create_iter_result_object(
    scope: &mut Scope<'_>,
    intr: &crate::Intrinsics,
    value: Value,
    done: bool,
  ) -> Result<GcObject, VmError> {
    scope.push_root(value)?;

    let out = scope.alloc_object()?;
    scope.push_root(Value::Object(out))?;
    scope
      .heap_mut()
      .object_set_prototype(out, Some(intr.object_prototype()))?;

    let value_key = super::string_key(scope, "value")?;
    let done_key = super::string_key(scope, "done")?;
    crate::spec_ops::create_data_property_or_throw(scope, out, value_key, value)?;
    crate::spec_ops::create_data_property_or_throw(scope, out, done_key, Value::Bool(done))?;
    Ok(out)
  }

  fn get_wrapper_method(
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    wrapper: Value,
    name: &str,
  ) -> Result<Value, VmError> {
    let Value::Object(wrapper_obj) = wrapper else {
      return Err(VmError::InvariantViolation(
        "expected wrapper iterator object",
      ));
    };
    // Root wrapper across key allocation + Get.
    let mut scope = scope.reborrow();
    scope.push_root(wrapper)?;

    let key = super::string_key(&mut scope, name)?;
    scope.get_with_host_and_hooks(vm, host, hooks, wrapper_obj, key, wrapper)
  }

  #[test]
  fn async_from_sync_iterator_return_does_not_close_on_rejected_value_when_not_done(
  ) -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host = TestHost::default();
    let mut hooks = MicrotaskQueue::new();

    let mut promise_root = None;

    let result: Result<(), VmError> = (|| {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      let boom_s = scope.alloc_string("boom")?;
      let boom = Value::String(boom_s);

      let rejected = super::promise_reject(&mut vm, &mut host, &mut hooks, &mut scope, boom)?;
      scope.push_root(rejected)?;

      let iter_result_obj = create_iter_result_object(&mut scope, &intr, rejected, false)?;
      let iter_result = Value::Object(iter_result_obj);

      let returns_slot0_id = vm.register_native_call(method_returns_slot0)?;
      let return_call_id = vm.register_native_call(return_increments_host_and_returns_slot0)?;

      let sync_next_name = scope.alloc_string("next")?;
      let sync_next = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        sync_next_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_next))?;

      let sync_return_name = scope.alloc_string("return")?;
      let sync_return = scope.alloc_native_function_with_slots(
        return_call_id,
        None,
        sync_return_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_return))?;

      let sync_iter = scope.alloc_object()?;
      scope.push_root(Value::Object(sync_iter))?;
      scope
        .heap_mut()
        .object_set_prototype(sync_iter, Some(intr.object_prototype()))?;

      let next_key = super::string_key(&mut scope, "next")?;
      crate::spec_ops::create_data_property_or_throw(
        &mut scope,
        sync_iter,
        next_key,
        Value::Object(sync_next),
      )?;
      let return_key = super::string_key(&mut scope, "return")?;
      crate::spec_ops::create_data_property_or_throw(
        &mut scope,
        sync_iter,
        return_key,
        Value::Object(sync_return),
      )?;

      let sync_record = IteratorRecord {
        iterator: Value::Object(sync_iter),
        next_method: Value::Object(sync_next),
        done: false,
      };

      let wrapper_record = super::create_async_from_sync_iterator(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        sync_record,
      )?;
      let wrapper = wrapper_record.iterator;

      let wrapper_return = get_wrapper_method(
        &mut vm, &mut host, &mut hooks, &mut scope, wrapper, "return",
      )?;

      let promise = vm.call(&mut host, &mut scope, wrapper_return, wrapper, &[])?;
      scope.push_root(promise)?;
      promise_root = Some(scope.heap_mut().add_root(promise)?);
      Ok(())
    })();

    if let Some(id) = promise_root {
      vm.perform_microtask_checkpoint_with_host(&mut host, &mut heap)?;
      let promise = heap.get_root(id).ok_or(VmError::InvariantViolation(
        "expected promise root to exist",
      ))?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("expected Promise object"));
      };
      assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
      let reason = heap
        .promise_result(promise_obj)?
        .ok_or(VmError::InvariantViolation("expected rejection reason"))?;
      assert_eq!(value_to_utf8_lossy(&heap, reason)?, "boom");
      assert_eq!(
        host.return_calls, 1,
        "AsyncFromSyncIterator.return must not IteratorClose on value rejection"
      );
      heap.remove_root(id);
    }

    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn async_from_sync_iterator_next_closes_on_rejected_value_when_not_done() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host = TestHost::default();
    let mut hooks = MicrotaskQueue::new();

    let mut promise_root = None;

    let result: Result<(), VmError> = (|| {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      let boom_s = scope.alloc_string("boom")?;
      let boom = Value::String(boom_s);

      let rejected = super::promise_reject(&mut vm, &mut host, &mut hooks, &mut scope, boom)?;
      scope.push_root(rejected)?;

      let iter_result_obj = create_iter_result_object(&mut scope, &intr, rejected, false)?;
      let iter_result = Value::Object(iter_result_obj);

      let returns_slot0_id = vm.register_native_call(method_returns_slot0)?;
      let return_call_id = vm.register_native_call(return_increments_host_and_returns_slot0)?;

      let sync_next_name = scope.alloc_string("next")?;
      let sync_next = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        sync_next_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_next))?;

      let sync_return_name = scope.alloc_string("return")?;
      let sync_return = scope.alloc_native_function_with_slots(
        return_call_id,
        None,
        sync_return_name,
        0,
        // IteratorClose ignores the return value for throw completions.
        &[Value::Undefined],
      )?;
      scope.push_root(Value::Object(sync_return))?;

      let sync_iter = scope.alloc_object()?;
      scope.push_root(Value::Object(sync_iter))?;
      scope
        .heap_mut()
        .object_set_prototype(sync_iter, Some(intr.object_prototype()))?;

      let return_key = super::string_key(&mut scope, "return")?;
      crate::spec_ops::create_data_property_or_throw(
        &mut scope,
        sync_iter,
        return_key,
        Value::Object(sync_return),
      )?;

      let sync_record = IteratorRecord {
        iterator: Value::Object(sync_iter),
        next_method: Value::Object(sync_next),
        done: false,
      };

      let wrapper_record = super::create_async_from_sync_iterator(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        sync_record,
      )?;
      let wrapper = wrapper_record.iterator;

      let promise = vm.call(
        &mut host,
        &mut scope,
        wrapper_record.next_method,
        wrapper,
        &[],
      )?;
      scope.push_root(promise)?;
      promise_root = Some(scope.heap_mut().add_root(promise)?);
      Ok(())
    })();

    if let Some(id) = promise_root {
      vm.perform_microtask_checkpoint_with_host(&mut host, &mut heap)?;
      let promise = heap.get_root(id).ok_or(VmError::InvariantViolation(
        "expected promise root to exist",
      ))?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("expected Promise object"));
      };
      assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
      let reason = heap
        .promise_result(promise_obj)?
        .ok_or(VmError::InvariantViolation("expected rejection reason"))?;
      assert_eq!(value_to_utf8_lossy(&heap, reason)?, "boom");
      assert_eq!(
        host.return_calls, 1,
        "AsyncFromSyncIterator.next must IteratorClose on value rejection"
      );
      heap.remove_root(id);
    }

    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn async_from_sync_iterator_close_on_rejection_respects_promise_resolve_abrupt(
  ) -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host = TestHost::default();
    let mut hooks = MicrotaskQueue::new();

    let result: Result<(), VmError> = (|| {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      let boom_s = scope.alloc_string("boom")?;
      let boom = Value::String(boom_s);
      scope.push_root(boom)?;

      let promise = crate::promise_ops::promise_resolve_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut host,
        &mut hooks,
        Value::Number(1.0),
      )?;
      scope.push_root(promise)?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("expected promise object"));
      };

      let throw_id = vm.register_native_call(throw_slot0)?;
      let getter_name = scope.alloc_string("")?;
      let getter_fn =
        scope.alloc_native_function_with_slots(throw_id, None, getter_name, 0, &[boom])?;
      scope.push_root(Value::Object(getter_fn))?;

      let ctor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(ctor_key_s))?;
      scope.define_property(
        promise_obj,
        PropertyKey::from_string(ctor_key_s),
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(getter_fn),
            set: Value::Undefined,
          },
        },
      )?;

      let iter_result_obj = create_iter_result_object(&mut scope, &intr, promise, false)?;
      let iter_result = Value::Object(iter_result_obj);

      let returns_slot0_id = vm.register_native_call(method_returns_slot0)?;
      let return_call_id = vm.register_native_call(return_increments_host_and_returns_slot0)?;

      let sync_next_name = scope.alloc_string("next")?;
      let sync_next = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        sync_next_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_next))?;

      let sync_throw_name = scope.alloc_string("throw")?;
      let sync_throw = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        sync_throw_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_throw))?;

      let sync_return_name = scope.alloc_string("return")?;
      let sync_return = scope.alloc_native_function_with_slots(
        return_call_id,
        None,
        sync_return_name,
        0,
        &[iter_result],
      )?;
      scope.push_root(Value::Object(sync_return))?;

      let sync_iter = scope.alloc_object()?;
      scope.push_root(Value::Object(sync_iter))?;
      scope
        .heap_mut()
        .object_set_prototype(sync_iter, Some(intr.object_prototype()))?;

      let return_key = super::string_key(&mut scope, "return")?;
      crate::spec_ops::create_data_property_or_throw(
        &mut scope,
        sync_iter,
        return_key,
        Value::Object(sync_return),
      )?;
      let throw_key = super::string_key(&mut scope, "throw")?;
      crate::spec_ops::create_data_property_or_throw(
        &mut scope,
        sync_iter,
        throw_key,
        Value::Object(sync_throw),
      )?;

      let sync_record = IteratorRecord {
        iterator: Value::Object(sync_iter),
        next_method: Value::Object(sync_next),
        done: false,
      };
      let wrapper_record = super::create_async_from_sync_iterator(
        &mut vm,
        &mut host,
        &mut hooks,
        &mut scope,
        sync_record,
      )?;
      let wrapper = wrapper_record.iterator;

      // `.next` uses closeOnRejection = true.
      host.return_calls = 0;
      let next_promise = vm.call(
        &mut host,
        &mut scope,
        wrapper_record.next_method,
        wrapper,
        &[],
      )?;
      let Value::Object(next_promise_obj) = next_promise else {
        return Err(VmError::InvariantViolation("expected Promise object"));
      };
      assert_eq!(
        scope.heap().promise_state(next_promise_obj)?,
        PromiseState::Rejected
      );
      assert_eq!(host.return_calls, 1);

      // `.throw` uses closeOnRejection = true.
      host.return_calls = 0;
      let wrapper_throw =
        get_wrapper_method(&mut vm, &mut host, &mut hooks, &mut scope, wrapper, "throw")?;
      let throw_promise = vm.call(&mut host, &mut scope, wrapper_throw, wrapper, &[])?;
      let Value::Object(throw_promise_obj) = throw_promise else {
        return Err(VmError::InvariantViolation("expected Promise object"));
      };
      assert_eq!(
        scope.heap().promise_state(throw_promise_obj)?,
        PromiseState::Rejected
      );
      assert_eq!(host.return_calls, 1);

      // `.return` uses closeOnRejection = false (no IteratorClose on PromiseResolve abrupt).
      host.return_calls = 0;
      let wrapper_return = get_wrapper_method(
        &mut vm, &mut host, &mut hooks, &mut scope, wrapper, "return",
      )?;
      let return_promise = vm.call(&mut host, &mut scope, wrapper_return, wrapper, &[])?;
      let Value::Object(return_promise_obj) = return_promise else {
        return Err(VmError::InvariantViolation("expected Promise object"));
      };
      assert_eq!(
        scope.heap().promise_state(return_promise_obj)?,
        PromiseState::Rejected
      );
      assert_eq!(host.return_calls, 1);

      Ok(())
    })();

    realm.teardown(&mut heap);
    result
  }
}
