use std::cell::{Cell, RefCell};

use vm_js::iterator::{async_iterator_close, async_iterator_next, get_async_iterator, iterator_complete, iterator_value};
use vm_js::{
  new_promise_capability_with_host_and_hooks, perform_promise_then, promise_resolve, GcObject, Heap,
  HeapLimits, MicrotaskQueue, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};

thread_local! {
  static ASYNC_ITERATOR_METHOD_CALLED: Cell<bool> = const { Cell::new(false) };
  static SYNC_ITERATOR_METHOD_CALLED: Cell<bool> = const { Cell::new(false) };
  static ARRAY_NEXT_OK: Cell<bool> = const { Cell::new(false) };
  static SYNC_RETURN_CALLED: Cell<bool> = const { Cell::new(false) };
  static CLOSE_PROMISE_FULFILLED: Cell<bool> = const { Cell::new(false) };
  static CLOSE_PROMISE_REJECTED: Cell<bool> = const { Cell::new(false) };
  static ASYNC_FROM_SYNC_REJECTION_REASON: RefCell<Option<String>> = RefCell::new(None);
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
  if slots.len() != 1 {
    return Err(VmError::InvariantViolation("expected 1 native slot"));
  }
  Ok(slots[0])
}

fn async_iterator_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  ASYNC_ITERATOR_METHOD_CALLED.with(|c| c.set(true));
  method_returns_slot0(vm, scope, host, hooks, callee, this, args)
}

fn sync_iterator_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  SYNC_ITERATOR_METHOD_CALLED.with(|c| c.set(true));
  method_returns_slot0(vm, scope, host, hooks, callee, this, args)
}

fn noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
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
    return Err(VmError::InvariantViolation("expected 1 native slot"));
  };
  Err(VmError::Throw(v))
}

fn record_async_from_sync_rejection_reason(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = match reason {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    other => format!("{other:?}"),
  };
  ASYNC_FROM_SYNC_REJECTION_REASON.with(|cell| {
    *cell.borrow_mut() = Some(s);
  });
  Ok(Value::Undefined)
}

fn promise_reject(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  reason: Value,
) -> Result<Value, VmError> {
  // Root the reason across promise construction and the reject call.
  let mut scope = scope.reborrow();
  scope.push_root(reason)?;

  let cap = new_promise_capability_with_host_and_hooks(vm, &mut scope, host_ctx, hooks)?;
  scope.push_roots(&[cap.promise, cap.reject, reason])?;

  let _ = vm.call_with_host_and_hooks(
    host_ctx,
    &mut scope,
    hooks,
    cap.reject,
    Value::Undefined,
    &[reason],
  )?;
  Ok(cap.promise)
}

fn check_array_next_iterator_result(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let iter_result = args.get(0).copied().unwrap_or(Value::Undefined);
  let done = iterator_complete(vm, host, hooks, scope, iter_result)?;
  let value = iterator_value(vm, host, hooks, scope, iter_result)?;
  ARRAY_NEXT_OK.with(|c| c.set(!done && value == Value::Number(1.0)));
  Ok(Value::Undefined)
}

fn sync_iterator_return(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  SYNC_RETURN_CALLED.with(|c| c.set(true));

  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "sync_iterator_return requires intrinsics (create a Realm first)",
  ))?;

  let mut scope = scope.reborrow();
  let out = scope.alloc_object()?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.object_prototype()))?;

  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  scope.create_data_property_or_throw(out, value_key, Value::Undefined)?;

  let done_key = PropertyKey::from_string(scope.alloc_string("done")?);
  scope.create_data_property_or_throw(out, done_key, Value::Bool(true))?;

  Ok(Value::Object(out))
}

fn on_close_fulfilled(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  CLOSE_PROMISE_FULFILLED.with(|c| c.set(true));
  Ok(Value::Undefined)
}

fn on_close_rejected(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  CLOSE_PROMISE_REJECTED.with(|c| c.set(true));
  Ok(Value::Undefined)
}

struct TestCtx<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
}

impl VmJobContext for TestCtx<'_> {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let mut scope = self.heap.scope();
    self.vm.call_with_host(&mut scope, host, callee, this, args)
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let mut scope = self.heap.scope();
    self
      .vm
      .construct_with_host(&mut scope, host, callee, args, new_target)
  }

  fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    self.heap.remove_root(id);
  }
}

#[test]
fn get_async_iterator_prefers_async_iterator_method() -> Result<(), VmError> {
  ASYNC_ITERATOR_METHOD_CALLED.with(|c| c.set(false));
  SYNC_ITERATOR_METHOD_CALLED.with(|c| c.set(false));

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host_ctx = ();
  let mut host = MicrotaskQueue::new();

  let result: Result<(), VmError> = (|| {
    let intr = realm.intrinsics();

    let mut scope = heap.scope();
    let iterable = scope.alloc_object()?;
    scope.push_root(Value::Object(iterable))?;
    scope
      .heap_mut()
      .object_set_prototype(iterable, Some(intr.object_prototype()))?;

    // Build an iterator object with a callable `next` method.
    let iterator_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iterator_obj))?;
    scope
      .heap_mut()
      .object_set_prototype(iterator_obj, Some(intr.object_prototype()))?;

    let next_call_id = vm.register_native_call(noop)?;
    let next_name = scope.alloc_string("next")?;
    let next_fn = scope.alloc_native_function(next_call_id, None, next_name, 0)?;
    let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
    scope.create_data_property_or_throw(iterator_obj, next_key, Value::Object(next_fn))?;

    // @@asyncIterator method returns `iterator_obj`.
    let async_iter_call_id = vm.register_native_call(async_iterator_method)?;
    let async_iter_name = scope.alloc_string("async_iter")?;
    let async_iter_fn = scope.alloc_native_function_with_slots(
      async_iter_call_id,
      None,
      async_iter_name,
      0,
      &[Value::Object(iterator_obj)],
    )?;

    // @@iterator method returns the same iterator object, but should never be called.
    let sync_iter_call_id = vm.register_native_call(sync_iterator_method)?;
    let sync_iter_name = scope.alloc_string("sync_iter")?;
    let sync_iter_fn = scope.alloc_native_function_with_slots(
      sync_iter_call_id,
      None,
      sync_iter_name,
      0,
      &[Value::Object(iterator_obj)],
    )?;

    let async_iter_key = PropertyKey::from_symbol(realm.well_known_symbols().async_iterator);
    scope.create_data_property_or_throw(iterable, async_iter_key, Value::Object(async_iter_fn))?;

    let iter_key = PropertyKey::from_symbol(realm.well_known_symbols().iterator);
    scope.create_data_property_or_throw(iterable, iter_key, Value::Object(sync_iter_fn))?;

    let _record = get_async_iterator(
      &mut vm,
      &mut host_ctx,
      &mut host,
      &mut scope,
      Value::Object(iterable),
    )?;

    Ok(())
  })();

  realm.teardown(&mut heap);

  result?;

  assert!(ASYNC_ITERATOR_METHOD_CALLED.with(|c| c.get()));
  assert!(!SYNC_ITERATOR_METHOD_CALLED.with(|c| c.get()));

  Ok(())
}

#[test]
fn get_async_iterator_sync_fallback_awaits_array_values() -> Result<(), VmError> {
  ARRAY_NEXT_OK.with(|c| c.set(false));

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host_ctx = ();
  let mut host = MicrotaskQueue::new();

  let result: Result<(), VmError> = (|| {
    let promise = {
      let mut scope = heap.scope();
      promise_resolve(&mut vm, &mut scope, &mut host, Value::Number(1.0))?
    };

    let next_promise = {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      // Root the Promise across subsequent allocations (array creation, property key strings, etc).
      // Otherwise, it is only held in a Rust local (not traced by GC) and can be collected if a GC
      // runs during allocation.
      let promise = scope.push_root(promise)?;

      let array = scope.alloc_array(0)?;
      scope.push_root(Value::Object(array))?;
      scope
        .heap_mut()
        .object_set_prototype(array, Some(intr.array_prototype()))?;

      let idx0 = PropertyKey::from_string(scope.alloc_string("0")?);
      scope.create_data_property_or_throw(array, idx0, promise)?;

      let record =
        get_async_iterator(&mut vm, &mut host_ctx, &mut host, &mut scope, Value::Object(array))?;
      async_iterator_next(&mut vm, &mut host_ctx, &mut host, &mut scope, &record)?
    };

    // Resolve the awaited IteratorResult and validate the unwrapped `value`.
    {
      let mut scope = heap.scope();
      // Root the promise returned by `AsyncIteratorNext` across the callback allocations and
      // PerformPromiseThen work. Otherwise it is only held by a Rust local and can be collected if a
      // GC runs during those allocations.
      let next_promise = scope.push_root(next_promise)?;
      let call_id = vm.register_native_call(check_array_next_iterator_result)?;
      let name = scope.alloc_string("check")?;
      let on_fulfilled = scope.alloc_native_function(call_id, None, name, 1)?;
      let _derived = perform_promise_then(
        &mut vm,
        &mut scope,
        &mut host,
        next_promise,
        Some(Value::Object(on_fulfilled)),
        None,
      )?;
    }

    let mut ctx = TestCtx { vm: &mut vm, heap: &mut heap };
    let errors = host.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "microtask checkpoint errors: {errors:?}");

    assert!(ARRAY_NEXT_OK.with(|c| c.get()));

    Ok(())
  })();

  realm.teardown(&mut heap);
  result
}

#[test]
fn async_iterator_close_invokes_sync_return() -> Result<(), VmError> {
  SYNC_RETURN_CALLED.with(|c| c.set(false));
  CLOSE_PROMISE_FULFILLED.with(|c| c.set(false));
  CLOSE_PROMISE_REJECTED.with(|c| c.set(false));

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host_ctx = ();
  let mut host = MicrotaskQueue::new();

  let result: Result<(), VmError> = (|| {
    let close_promise = {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      // Create a sync iterator object with `next` and `return`.
      let sync_iter = scope.alloc_object()?;
      scope.push_root(Value::Object(sync_iter))?;
      scope
        .heap_mut()
        .object_set_prototype(sync_iter, Some(intr.object_prototype()))?;

      let next_call_id = vm.register_native_call(noop)?;
      let next_name = scope.alloc_string("next")?;
      let next_fn = scope.alloc_native_function(next_call_id, None, next_name, 0)?;
      let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
      scope.create_data_property_or_throw(sync_iter, next_key, Value::Object(next_fn))?;

      let return_call_id = vm.register_native_call(sync_iterator_return)?;
      let return_name = scope.alloc_string("return")?;
      let return_fn = scope.alloc_native_function(return_call_id, None, return_name, 0)?;
      let return_key = PropertyKey::from_string(scope.alloc_string("return")?);
      scope.create_data_property_or_throw(sync_iter, return_key, Value::Object(return_fn))?;

      // Create an iterable that returns the sync iterator from @@iterator.
      let iterable = scope.alloc_object()?;
      scope.push_root(Value::Object(iterable))?;
      scope
        .heap_mut()
        .object_set_prototype(iterable, Some(intr.object_prototype()))?;

      let iter_call_id = vm.register_native_call(method_returns_slot0)?;
      let iter_name = scope.alloc_string("iter")?;
      let iter_fn = scope.alloc_native_function_with_slots(
        iter_call_id,
        None,
        iter_name,
        0,
        &[Value::Object(sync_iter)],
      )?;
      let iter_key = PropertyKey::from_symbol(realm.well_known_symbols().iterator);
      scope.create_data_property_or_throw(iterable, iter_key, Value::Object(iter_fn))?;

      let record =
        get_async_iterator(&mut vm, &mut host_ctx, &mut host, &mut scope, Value::Object(iterable))?;
      async_iterator_close(&mut vm, &mut host_ctx, &mut host, &mut scope, &record)?
    };

    // Sync `return` should have been called synchronously by `AsyncFromSyncIterator.prototype.return`.
    assert!(SYNC_RETURN_CALLED.with(|c| c.get()));

    // Attach handlers to ensure the returned promise is fulfilled.
    {
      let mut scope = heap.scope();
      let fulfilled_id = vm.register_native_call(on_close_fulfilled)?;
      let rejected_id = vm.register_native_call(on_close_rejected)?;
      let ok_name = scope.alloc_string("ok")?;
      let err_name = scope.alloc_string("err")?;
      let on_fulfilled = scope.alloc_native_function(fulfilled_id, None, ok_name, 1)?;
      let on_rejected = scope.alloc_native_function(rejected_id, None, err_name, 1)?;
      let _derived = perform_promise_then(
        &mut vm,
        &mut scope,
        &mut host,
        close_promise,
        Some(Value::Object(on_fulfilled)),
        Some(Value::Object(on_rejected)),
      )?;
    }

    let mut ctx = TestCtx { vm: &mut vm, heap: &mut heap };
    let errors = host.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "microtask checkpoint errors: {errors:?}");

    assert!(CLOSE_PROMISE_FULFILLED.with(|c| c.get()));
    assert!(!CLOSE_PROMISE_REJECTED.with(|c| c.get()));

    Ok(())
  })();

  realm.teardown(&mut heap);
  result
}

#[test]
fn async_from_sync_iterator_continuation_promise_resolve_throw_suppresses_iterator_close_error(
) -> Result<(), VmError> {
  ASYNC_FROM_SYNC_REJECTION_REASON.with(|cell| {
    cell.borrow_mut().take();
  });

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host_ctx = ();
  let mut host = MicrotaskQueue::new();

  let result: Result<(), VmError> = (|| {
    let intr = realm.intrinsics();
    {
      let mut scope = heap.scope();

      // Promise that throws when `PromiseResolve(%Promise%, value)` reads `value.constructor`.
      let promise = promise_resolve(&mut vm, &mut scope, &mut host, Value::Number(1.0))?;
      scope.push_root(promise)?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("expected promise object"));
      };

      // Define a throwing `constructor` getter on the promise.
      let throw_id = vm.register_native_call(throw_slot0)?;
      let getter_name = scope.alloc_string("")?;
      let thrown_s = scope.alloc_string("promiseResolve")?;
      scope.push_root(Value::String(thrown_s))?;
      let getter_fn = scope.alloc_native_function_with_slots(
        throw_id,
        None,
        getter_name,
        0,
        &[Value::String(thrown_s)],
      )?;
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
      // - `return()` throwing "close".
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

      let close_s = scope.alloc_string("close")?;
      scope.push_root(Value::String(close_s))?;
      let return_name = scope.alloc_string("return")?;
      scope.push_root(Value::String(return_name))?;
      let return_fn = scope.alloc_native_function_with_slots(
        throw_id,
        None,
        return_name,
        0,
        &[Value::String(close_s)],
      )?;
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
      let iter_key = PropertyKey::from_symbol(realm.well_known_symbols().iterator);
      scope.create_data_property_or_throw(iterable, iter_key, Value::Object(iter_fn))?;

      // Create the async-from-sync wrapper and call `next()`.
      let record = get_async_iterator(
        &mut vm,
        &mut host_ctx,
        &mut host,
        &mut scope,
        Value::Object(iterable),
      )?;
      let next_promise =
        async_iterator_next(&mut vm, &mut host_ctx, &mut host, &mut scope, &record)?;

      // Observe the rejection reason.
      let on_rejected_id = vm.register_native_call(record_async_from_sync_rejection_reason)?;
      let rejected_name = scope.alloc_string("onRejected")?;
      scope.push_root(Value::String(rejected_name))?;
      let on_rejected_fn =
        scope.alloc_native_function(on_rejected_id, None, rejected_name, 1)?;
      scope.push_root(Value::Object(on_rejected_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(on_rejected_fn, Some(intr.function_prototype()))?;

      let _derived = perform_promise_then(
        &mut vm,
        &mut scope,
        &mut host,
        next_promise,
        None,
        Some(Value::Object(on_rejected_fn)),
      )?;
    }

    let mut ctx = TestCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    let errors = host.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "microtask checkpoint errors: {errors:?}");

    Ok(())
  })();

  realm.teardown(&mut heap);
  result?;

  let reason = ASYNC_FROM_SYNC_REJECTION_REASON.with(|cell| cell.borrow_mut().take());
  assert_eq!(reason.as_deref(), Some("promiseResolve"));
  Ok(())
}

#[test]
fn async_from_sync_iterator_close_iterator_suppresses_iterator_close_error() -> Result<(), VmError> {
  ASYNC_FROM_SYNC_REJECTION_REASON.with(|cell| {
    cell.borrow_mut().take();
  });

  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host_ctx = ();
  let mut host = MicrotaskQueue::new();

  let result: Result<(), VmError> = (|| {
    let intr = realm.intrinsics();
    {
      let mut scope = heap.scope();

      // Rejected promise used as the iterator result `value`.
      let reason_s = scope.alloc_string("reason")?;
      scope.push_root(Value::String(reason_s))?;
      let rejected_value = Value::String(reason_s);
      let rejected_promise =
        promise_reject(&mut vm, &mut scope, &mut host_ctx, &mut host, rejected_value)?;
      scope.push_root(rejected_promise)?;

      // Build a sync iterator result object `{ value: rejected_promise, done: false }`.
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
        rejected_promise,
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
      // - `return()` throwing "close" (which must be suppressed).
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

      let throw_id = vm.register_native_call(throw_slot0)?;
      let close_s = scope.alloc_string("close")?;
      scope.push_root(Value::String(close_s))?;
      let return_name = scope.alloc_string("return")?;
      scope.push_root(Value::String(return_name))?;
      let return_fn = scope.alloc_native_function_with_slots(
        throw_id,
        None,
        return_name,
        0,
        &[Value::String(close_s)],
      )?;
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
      let iter_key = PropertyKey::from_symbol(realm.well_known_symbols().iterator);
      scope.create_data_property_or_throw(iterable, iter_key, Value::Object(iter_fn))?;

      // Create the async-from-sync wrapper and call `next()`.
      let record = get_async_iterator(
        &mut vm,
        &mut host_ctx,
        &mut host,
        &mut scope,
        Value::Object(iterable),
      )?;
      let next_promise =
        async_iterator_next(&mut vm, &mut host_ctx, &mut host, &mut scope, &record)?;

      // Observe the rejection reason.
      let on_rejected_id = vm.register_native_call(record_async_from_sync_rejection_reason)?;
      let rejected_name = scope.alloc_string("onRejected")?;
      scope.push_root(Value::String(rejected_name))?;
      let on_rejected_fn =
        scope.alloc_native_function(on_rejected_id, None, rejected_name, 1)?;
      scope.push_root(Value::Object(on_rejected_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(on_rejected_fn, Some(intr.function_prototype()))?;

      let _derived = perform_promise_then(
        &mut vm,
        &mut scope,
        &mut host,
        next_promise,
        None,
        Some(Value::Object(on_rejected_fn)),
      )?;
    }

    let mut ctx = TestCtx {
      vm: &mut vm,
      heap: &mut heap,
    };
    let errors = host.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "microtask checkpoint errors: {errors:?}");

    Ok(())
  })();

  realm.teardown(&mut heap);
  result?;

  let reason = ASYNC_FROM_SYNC_REJECTION_REASON.with(|cell| cell.borrow_mut().take());
  assert_eq!(reason.as_deref(), Some("reason"));
  Ok(())
}
