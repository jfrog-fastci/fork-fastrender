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

      let result = match iterator::iterator_next(vm, host, hooks, &mut next_scope, sync, None) {
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
            // Root the pending thrown value across `IteratorClose`, which can allocate and trigger
            // GC. Values on the Rust stack (including `VmError`) are not traced.
            let original_is_throw = err.is_throw_completion();
            if original_is_throw {
              if let Some(thrown) = err.thrown_value() {
                next_scope.push_root(thrown)?;
              }
            }

            let record = iterator::IteratorRecord {
              iterator: sync.iterator,
              next_method: Value::Undefined,
              done: false,
            };
            let close_res = iterator::iterator_close(
              vm,
              host,
              hooks,
              &mut next_scope,
              &record,
              iterator::CloseCompletionKind::Throw,
            );
            if let Err(close_err) = close_res {
              // `IteratorClose` suppression rules:
              // - If the original completion is a throw completion, suppress JS-visible close
              //   errors.
              // - Never suppress fatal VM errors (OOM/termination/etc).
              if original_is_throw && !close_err.is_throw_completion() {
                return reject_promise_from_vm_error(vm, host, hooks, &mut next_scope, close_err);
              }
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::GcObject;
  use crate::property::{PropertyDescriptor, PropertyKind};
  use crate::{Heap, HeapLimits, MicrotaskQueue, Realm, VmOptions};

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

  fn throw_new_object_with_tag_promise_resolve(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    // Allocate a fresh object each call so the thrown value is *not* reachable from any other root.
    // This lets the test detect missing rooting by forcing a GC during `IteratorClose`.
    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "throw_new_object_with_tag_promise_resolve requires intrinsics (create a Realm first)",
    ))?;
    let mut scope = scope.reborrow();

    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(intr.object_prototype()))?;

    let tag_key_s = scope.alloc_string("tag")?;
    scope.push_root(Value::String(tag_key_s))?;
    let tag_key = PropertyKey::from_string(tag_key_s);

    let tag_value_s = scope.alloc_string("promiseResolve")?;
    scope.push_root(Value::String(tag_value_s))?;
    scope.create_data_property_or_throw(obj, tag_key, Value::String(tag_value_s))?;

    Err(VmError::Throw(Value::Object(obj)))
  }

  fn gc_and_return_object(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    // Force a full GC cycle during `IteratorClose` so thrown values that are not rooted (values on
    // the Rust stack are not traced) will be collected.
    scope.heap_mut().collect_garbage();

    let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
      "gc_and_return_object requires intrinsics (create a Realm first)",
    ))?;
    let mut scope = scope.reborrow();
    let obj = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(intr.object_prototype()))?;
    Ok(Value::Object(obj))
  }

  #[test]
  fn async_from_sync_iterator_continuation_roots_thrown_value_during_iterator_close() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    // This test allocates a non-trivial Promise + iterator graph, and the intrinsic graph grows as
    // vm-js gains more built-ins. Keep this small (to encourage frequent GC) but large enough that
    // intrinsic initialization doesn't starve the test allocations.
    let mut heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host_ctx = ();
    let mut hooks = MicrotaskQueue::new();

    let result: Result<(), VmError> = (|| {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      // Promise used as the iterator result `value`.
      let promise = promise_ops::promise_resolve_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut hooks,
        Value::Number(1.0),
      )?;
      scope.push_root(promise)?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("expected promise object"));
      };

      // Define a throwing `constructor` getter on the promise. The getter throws a *fresh object*
      // with `tag: "promiseResolve"`.
      let throw_id = vm.register_native_call(throw_new_object_with_tag_promise_resolve)?;
      let getter_name = scope.alloc_string("")?;
      let getter_fn = scope.alloc_native_function(throw_id, None, getter_name, 0)?;
      scope.push_root(Value::Object(getter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(getter_fn, Some(intr.function_prototype()))?;

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

      // Build a sync iterator result object `{ value: promise, done: false }`.
      let iter_result_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(iter_result_obj))?;
      scope
        .heap_mut()
        .object_set_prototype(iter_result_obj, Some(intr.object_prototype()))?;

      let value_key_s = scope.alloc_string("value")?;
      scope.push_root(Value::String(value_key_s))?;
      scope.create_data_property_or_throw(
        iter_result_obj,
        PropertyKey::from_string(value_key_s),
        promise,
      )?;

      let done_key_s = scope.alloc_string("done")?;
      scope.push_root(Value::String(done_key_s))?;
      scope.create_data_property_or_throw(
        iter_result_obj,
        PropertyKey::from_string(done_key_s),
        Value::Bool(false),
      )?;

      // Create a sync iterator object with:
      // - `next()` returning `iter_result_obj`, and
      // - `return()` forcing a GC.
      let sync_iter = scope.alloc_object()?;
      scope.push_root(Value::Object(sync_iter))?;
      scope
        .heap_mut()
        .object_set_prototype(sync_iter, Some(intr.object_prototype()))?;

      let returns_slot0_id = vm.register_native_call(method_returns_slot0)?;
      let next_name = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_name))?;
      let next_fn = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        next_name,
        0,
        &[Value::Object(iter_result_obj)],
      )?;
      scope.push_root(Value::Object(next_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(next_fn, Some(intr.function_prototype()))?;
      scope.create_data_property_or_throw(
        sync_iter,
        PropertyKey::from_string(next_name),
        Value::Object(next_fn),
      )?;

      let return_call_id = vm.register_native_call(gc_and_return_object)?;
      let return_name = scope.alloc_string("return")?;
      scope.push_root(Value::String(return_name))?;
      let return_fn = scope.alloc_native_function(return_call_id, None, return_name, 0)?;
      scope.push_root(Value::Object(return_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(return_fn, Some(intr.function_prototype()))?;
      scope.create_data_property_or_throw(
        sync_iter,
        PropertyKey::from_string(return_name),
        Value::Object(return_fn),
      )?;

      // Create an iterable that returns `sync_iter` from @@iterator.
      let iterable = scope.alloc_object()?;
      scope.push_root(Value::Object(iterable))?;
      scope
        .heap_mut()
        .object_set_prototype(iterable, Some(intr.object_prototype()))?;

      let iter_name = scope.alloc_string("iter")?;
      scope.push_root(Value::String(iter_name))?;
      let iter_fn = scope.alloc_native_function_with_slots(
        returns_slot0_id,
        None,
        iter_name,
        0,
        &[Value::Object(sync_iter)],
      )?;
      scope.push_root(Value::Object(iter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(iter_fn, Some(intr.function_prototype()))?;
      let iter_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
      scope.create_data_property_or_throw(iterable, iter_key, Value::Object(iter_fn))?;

      // Create the async-from-sync wrapper and call `next()`.
      let mut record = get_async_iterator(
        &mut vm,
        &mut host_ctx,
        &mut hooks,
        &mut scope,
        Value::Object(iterable),
      )?;
      let next_promise = async_iterator_next(&mut vm, &mut host_ctx, &mut hooks, &mut scope, &mut record)?;

      let Value::Object(next_promise_obj) = next_promise else {
        return Err(VmError::InvariantViolation("expected promise object"));
      };
      assert_eq!(scope.heap().promise_state(next_promise_obj)?, crate::PromiseState::Rejected);
      let Some(reason) = scope.heap().promise_result(next_promise_obj)? else {
        return Err(VmError::InvariantViolation("expected rejected promise reason"));
      };

      // Validate that the rejection reason object survived the GC performed during iterator close.
      let Value::Object(reason_obj) = reason else {
        return Err(VmError::InvariantViolation("expected object rejection reason"));
      };
      let mut check_scope = scope.reborrow();
      check_scope.push_root(reason)?;

      let tag_key = super::string_key(&mut check_scope, "tag")?;
      let tag = check_scope.get_with_host_and_hooks(
        &mut vm,
        &mut host_ctx,
        &mut hooks,
        reason_obj,
        tag_key,
        Value::Object(reason_obj),
      )?;
      let Value::String(tag_s) = tag else {
        return Err(VmError::InvariantViolation("expected string tag"));
      };
      let tag = check_scope.heap().get_string(tag_s)?.to_utf8_lossy();
      assert_eq!(tag, "promiseResolve");

      Ok(())
    })();

    realm.teardown(&mut heap);
    result
  }
}
