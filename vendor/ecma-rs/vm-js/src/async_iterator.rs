use crate::property::PropertyKey;
use crate::{iterator, promise_ops, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// ECMAScript "AsyncIteratorRecord" (ECMA-262).
///
/// This mirrors the spec shape of an iterator record while also supporting async-from-sync
/// iteration by carrying an underlying synchronous [`iterator::IteratorRecord`].
#[derive(Debug, Clone, Copy)]
pub enum AsyncIteratorRecord {
  /// Protocol async iterators returned by `@@asyncIterator`.
  Protocol {
    iterator: Value,
    next_method: Value,
    #[allow(dead_code)]
    done: bool,
  },
  /// Async-from-sync wrapper semantics (`CreateAsyncFromSyncIterator` / `SyncIteratorToAsyncIterator`).
  Sync {
    sync: iterator::IteratorRecord,
  },
}

impl AsyncIteratorRecord {
  #[inline]
  pub fn iterator(&self) -> Value {
    match self {
      AsyncIteratorRecord::Protocol { iterator, .. } => *iterator,
      AsyncIteratorRecord::Sync { sync } => sync.iterator,
    }
  }

  #[inline]
  pub fn next_method(&self) -> Value {
    match self {
      AsyncIteratorRecord::Protocol { next_method, .. } => *next_method,
      AsyncIteratorRecord::Sync { sync } => sync.next_method,
    }
  }
}

fn string_key(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  // Root the key string across any subsequent operations (property lookup/definition can allocate
  // and trigger GC, and values on the Rust stack are not traced).
  let key_s = scope.alloc_string(s)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
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

  let cap = promise_ops::new_promise_capability_with_host_and_hooks(vm, &mut scope, host, hooks)?;

  // Root the resolving functions + reason across the reject call (can allocate / run user JS).
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

/// `GetAsyncIterator` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-getasynciterator>
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

  let async_iterator_sym = intr.well_known_symbols().async_iterator;
  if let Some(method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    iterable,
    PropertyKey::from_symbol(async_iterator_sym),
  )? {
    return get_async_iterator_from_method(vm, host, hooks, scope, iterable, method);
  }

  // Fall back to sync iterator protocol and wrap.
  let sync = iterator::get_iterator_protocol(vm, host, hooks, scope, iterable)?;
  Ok(create_async_from_sync_iterator(sync))
}

/// `GetAsyncIteratorFromMethod` (ECMA-262).
pub fn get_async_iterator_from_method(
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
      "GetAsyncIteratorFromMethod: async iterator method did not return an object",
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
      "GetAsyncIteratorFromMethod: iterator.next is not callable",
    ));
  }

  Ok(AsyncIteratorRecord::Protocol {
    iterator,
    next_method: next,
    done: false,
  })
}

/// `CreateAsyncFromSyncIterator` (ECMA-262).
///
/// This returns a record representing the async-from-sync wrapper semantics.
#[inline]
pub fn create_async_from_sync_iterator(sync: iterator::IteratorRecord) -> AsyncIteratorRecord {
  AsyncIteratorRecord::Sync { sync }
}

/// `AsyncIteratorNext` (ECMA-262).
///
/// Note: this returns the *raw* result of calling the iterator's `next` method (which the caller
/// should `await`).
pub fn async_iterator_next(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &mut AsyncIteratorRecord,
) -> Result<Value, VmError> {
  match record {
    AsyncIteratorRecord::Protocol {
      iterator,
      next_method,
      ..
    } => {
      let mut call_scope = scope.reborrow();
      call_scope.push_roots(&[*iterator, *next_method])?;
      vm.call_with_host_and_hooks(host, &mut call_scope, hooks, *next_method, *iterator, &[])
    }
    AsyncIteratorRecord::Sync { sync } => {
      let mut next_scope = scope.reborrow();
      next_scope.push_roots(&[sync.iterator, sync.next_method])?;

      let result = match iterator::iterator_next(vm, host, hooks, &mut next_scope, sync) {
        Ok(v) => v,
        Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, err),
      };

      if !matches!(result, Value::Object(_)) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("AsyncFromSyncIterator requires intrinsics"))?;
        let reason = crate::error_object::new_type_error_object(
          &mut next_scope,
          &intr,
          "AsyncFromSyncIterator.next: IteratorNext returned non-object",
        )?;
        return promise_reject(vm, host, hooks, &mut next_scope, reason);
      }

      // Root the iterator result object while reading `done`/`value`.
      next_scope.push_root(result)?;
      let done = match iterator::iterator_complete(vm, host, hooks, &mut next_scope, result) {
        Ok(d) => d,
        Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, err),
      };
      let value = match iterator::iterator_value(vm, host, hooks, &mut next_scope, result) {
        Ok(v) => v,
        Err(err) => return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, err),
      };
      next_scope.push_root(value)?;

      let value_wrapper = match promise_ops::promise_resolve_with_host_and_hooks(
        vm,
        &mut next_scope,
        host,
        hooks,
        value,
      ) {
        Ok(p) => p,
        Err(err) => {
          if !done {
            let record = iterator::IteratorRecord {
              iterator: sync.iterator,
              next_method: Value::Undefined,
              done: false,
            };
            if let Err(close_err) = iterator::iterator_close(vm, host, hooks, &mut next_scope, &record) {
              return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, close_err);
            }
          }
          return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, err);
        }
      };
      next_scope.push_root(value_wrapper)?;

      let unwrap_call_id = vm.async_from_sync_iterator_unwrap_call_id()?;
      let unwrap_name = next_scope.alloc_string("")?;
      let unwrap = next_scope.alloc_native_function_with_slots(
        unwrap_call_id,
        None,
        unwrap_name,
        1,
        &[Value::Bool(done)],
      )?;
      next_scope.push_root(Value::Object(unwrap))?;

      let on_rejected = if done {
        None
      } else {
        let close_call_id = vm.async_from_sync_iterator_close_call_id()?;
        let close_name = next_scope.alloc_string("")?;
        let close = next_scope.alloc_native_function_with_slots(
          close_call_id,
          None,
          close_name,
          1,
          &[sync.iterator],
        )?;
        next_scope.push_root(Value::Object(close))?;
        Some(Value::Object(close))
      };

      promise_ops::perform_promise_then_with_host_and_hooks(
        vm,
        &mut next_scope,
        host,
        hooks,
        value_wrapper,
        Some(Value::Object(unwrap)),
        on_rejected,
      )
    }
  }
}

/// `AsyncIteratorClose` (ECMA-262) (partial).
///
/// Returns `Ok(None)` when the iterator has no `return` method. Otherwise returns the raw result of
/// calling `return` (which the caller should `await`).
pub fn async_iterator_close(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  record: &AsyncIteratorRecord,
) -> Result<Option<Value>, VmError> {
  // Root the iterator while allocating the `"return"` key and performing the `GetMethod` / `Call`
  // sequence (both can allocate and trigger GC).
  let iterator = record.iterator();
  let mut close_scope = scope.reborrow();
  close_scope.push_root(iterator)?;

  let return_key = string_key(&mut close_scope, "return")?;
  let Some(return_method) = crate::spec_ops::get_method_with_host_and_hooks(
    vm,
    &mut close_scope,
    host,
    hooks,
    iterator,
    return_key,
  )?
  else {
    return Ok(None);
  };

  let result =
    vm.call_with_host_and_hooks(host, &mut close_scope, hooks, return_method, iterator, &[])?;
  Ok(Some(result))
}
