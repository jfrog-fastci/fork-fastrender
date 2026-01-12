use crate::error_object::new_type_error_object;
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
  pub fn is_sync(&self) -> bool {
    matches!(self, AsyncIteratorRecord::Sync { .. })
  }

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

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let err = new_type_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(err))
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
  let method =
    get_method(vm, host, hooks, scope, iterable, PropertyKey::from_symbol(async_iterator_sym))?;
  if let Some(method) = method {
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
    return Err(throw_type_error(
      vm,
      scope,
      "GetAsyncIteratorFromMethod: async iterator method did not return an object",
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
      "GetAsyncIteratorFromMethod: iterator.next is not callable",
    )?);
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

fn create_iter_result_object(
  scope: &mut Scope<'_>,
  value: Value,
  done: bool,
) -> Result<Value, VmError> {
  // Root the inputs across allocation and property definition.
  let mut scope = scope.reborrow();
  scope.push_roots(&[value])?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let value_key = string_key(&mut scope, "value")?;
  scope.create_data_property_or_throw(obj, value_key, value)?;

  let done_key = string_key(&mut scope, "done")?;
  scope.create_data_property_or_throw(obj, done_key, Value::Bool(done))?;

  Ok(Value::Object(obj))
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
      let next_value = iterator::iterator_step_value(vm, host, hooks, scope, sync)?;
      let done = next_value.is_none();
      let value = next_value.unwrap_or(Value::Undefined);

      let iter_result = create_iter_result_object(scope, value, done)?;
      let mut promise_scope = scope.reborrow();
      promise_scope.push_root(iter_result)?;
      promise_ops::promise_resolve_with_host_and_hooks(vm, &mut promise_scope, host, hooks, iter_result)
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
  let Some(return_method) = get_method(vm, host, hooks, &mut close_scope, iterator, return_key)? else {
    return Ok(None);
  };

  let result =
    vm.call_with_host_and_hooks(host, &mut close_scope, hooks, return_method, iterator, &[])?;
  Ok(Some(result))
}
