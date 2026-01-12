use vm_js::iterator::{self, CloseCompletionKind};
use vm_js::{
  GcObject, Heap, HeapLimits, Job, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

#[derive(Debug, Default)]
struct NoopHooks;

impl VmHostHooks for NoopHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
    // Not needed for these tests.
  }
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn return_slot0(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  Ok(slots.get(0).copied().unwrap_or(Value::Undefined))
}

fn throw_1(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::Throw(Value::Number(1.0)))
}

#[test]
fn iterator_step_value_sets_done_true_when_done_getter_throws() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host = ();
  let mut hooks = NoopHooks::default();

  let mut scope = heap.scope();

  let throw_id = vm.register_native_call(throw_1)?;
  let throw_name = scope.alloc_string("throw")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
  scope.push_root(Value::Object(throw_fn))?;

  // Iterator result object whose `done` getter throws.
  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  let done_key = PropertyKey::from_string(scope.alloc_string("done")?);
  scope.define_property(
    iter_result,
    done_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(throw_fn),
        set: Value::Undefined,
      },
    },
  )?;
  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  scope.define_property(iter_result, value_key, data_desc(Value::Number(123.0)))?;

  // `next()` returns the iterator result object.
  let return_slot0_id = vm.register_native_call(return_slot0)?;
  let next_name = scope.alloc_string("next")?;
  let next_fn = scope.alloc_native_function_with_slots(
    return_slot0_id,
    None,
    next_name,
    0,
    &[Value::Object(iter_result)],
  )?;
  scope.push_root(Value::Object(next_fn))?;

  // Iterator object.
  let iterator_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(iterator_obj))?;
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  scope.define_property(iterator_obj, next_key, data_desc(Value::Object(next_fn)))?;

  // Iterator method returns the iterator object.
  let iter_method_name = scope.alloc_string("@@iterator")?;
  let iter_method = scope.alloc_native_function_with_slots(
    return_slot0_id,
    None,
    iter_method_name,
    0,
    &[Value::Object(iterator_obj)],
  )?;
  scope.push_root(Value::Object(iter_method))?;

  let mut record = iterator::get_iterator_from_method(
    &mut vm,
    &mut host,
    &mut hooks,
    &mut scope,
    Value::Undefined,
    Value::Object(iter_method),
  )?;
  scope.push_roots(&[record.iterator, record.next_method])?;

  let err = iterator::iterator_step_value(&mut vm, &mut host, &mut hooks, &mut scope, &mut record)
    .expect_err("done getter should throw");
  assert_eq!(err.thrown_value(), Some(Value::Number(1.0)));
  assert!(
    record.done,
    "IteratorStepValue must set IteratorRecord.done=true when done getter throws"
  );

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn iterator_step_value_sets_done_true_when_value_getter_throws() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host = ();
  let mut hooks = NoopHooks::default();

  let mut scope = heap.scope();

  let throw_id = vm.register_native_call(throw_1)?;
  let throw_name = scope.alloc_string("throw")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;

  // Iterator result object whose `value` getter throws.
  let iter_result = scope.alloc_object()?;
  let done_key = PropertyKey::from_string(scope.alloc_string("done")?);
  scope.define_property(iter_result, done_key, data_desc(Value::Bool(false)))?;
  let value_key = PropertyKey::from_string(scope.alloc_string("value")?);
  scope.define_property(
    iter_result,
    value_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(throw_fn),
        set: Value::Undefined,
      },
    },
  )?;

  // `next()` returns the iterator result object.
  let return_slot0_id = vm.register_native_call(return_slot0)?;
  let next_name = scope.alloc_string("next")?;
  let next_fn = scope.alloc_native_function_with_slots(
    return_slot0_id,
    None,
    next_name,
    0,
    &[Value::Object(iter_result)],
  )?;

  // Iterator object.
  let iterator_obj = scope.alloc_object()?;
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  scope.define_property(iterator_obj, next_key, data_desc(Value::Object(next_fn)))?;

  // Iterator method returns the iterator object.
  let iter_method_name = scope.alloc_string("@@iterator")?;
  let iter_method = scope.alloc_native_function_with_slots(
    return_slot0_id,
    None,
    iter_method_name,
    0,
    &[Value::Object(iterator_obj)],
  )?;

  let mut record = iterator::get_iterator_from_method(
    &mut vm,
    &mut host,
    &mut hooks,
    &mut scope,
    Value::Undefined,
    Value::Object(iter_method),
  )?;
  scope.push_roots(&[record.iterator, record.next_method])?;

  let err = iterator::iterator_step_value(&mut vm, &mut host, &mut hooks, &mut scope, &mut record)
    .expect_err("value getter should throw");
  assert_eq!(err.thrown_value(), Some(Value::Number(1.0)));
  assert!(
    record.done,
    "IteratorStepValue must set IteratorRecord.done=true when value getter throws"
  );

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn iterator_close_propagates_get_method_error_for_throw_completion() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host = ();
  let mut hooks = NoopHooks::default();

  let result: Result<(), VmError> = (|| {
    let mut scope = heap.scope();

    let throw_id = vm.register_native_call(throw_1)?;
    let throw_name = scope.alloc_string("throw")?;
    let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
    scope.push_root(Value::Object(throw_fn))?;

    // Iterator object with a callable `next` (required by GetIteratorFromMethod), and a `"return"`
    // accessor getter that throws.
    let return_slot0_id = vm.register_native_call(return_slot0)?;
    let next_name = scope.alloc_string("next")?;
    let next_fn = scope.alloc_native_function(return_slot0_id, None, next_name, 0)?;
    scope.push_root(Value::Object(next_fn))?;

    let iterator_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iterator_obj))?;
    let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
    scope.define_property(iterator_obj, next_key, data_desc(Value::Object(next_fn)))?;

    let return_key = PropertyKey::from_string(scope.alloc_string("return")?);
    scope.define_property(
      iterator_obj,
      return_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(throw_fn),
          set: Value::Undefined,
        },
      },
    )?;

    let iter_method_name = scope.alloc_string("@@iterator")?;
    let iter_method = scope.alloc_native_function_with_slots(
      return_slot0_id,
      None,
      iter_method_name,
      0,
      &[Value::Object(iterator_obj)],
    )?;
    scope.push_root(Value::Object(iter_method))?;

    let record = iterator::get_iterator_from_method(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      Value::Undefined,
      Value::Object(iter_method),
    )?;

    // Throw completion: closing errors from `GetMethod(iterator, "return")` must still propagate and
    // override the incoming completion.
    let err = iterator::iterator_close(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      &record,
      CloseCompletionKind::Throw,
    )
    .expect_err("expected IteratorClose to propagate GetMethod error for Throw completion");
    assert_eq!(err.thrown_value(), Some(Value::Number(1.0)));

    // Non-throw completion: closing errors must also propagate.
    let err = iterator::iterator_close(
      &mut vm,
      &mut host,
      &mut hooks,
      &mut scope,
      &record,
      CloseCompletionKind::NonThrow,
    )
    .expect_err("expected IteratorClose to propagate GetMethod error for NonThrow completion");
    assert_eq!(err.thrown_value(), Some(Value::Number(1.0)));
    Ok(())
  })();

  realm.teardown(&mut heap);
  result
}

#[derive(Debug, Default)]
struct CounterHost {
  count: u32,
}

fn inc_host_and_return_number(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host
    .as_any_mut()
    .downcast_mut::<CounterHost>()
    .ok_or(VmError::Unimplemented("host has unexpected type"))?;
  host.count = host.count.saturating_add(1);
  Ok(Value::Number(0.0))
}

#[test]
fn iterator_close_non_object_return_value_ignored_for_throw_completion() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut host = CounterHost::default();
  let mut hooks = NoopHooks::default();

  let mut scope = heap.scope();

  let return_slot0_id = vm.register_native_call(return_slot0)?;
  let next_name = scope.alloc_string("next")?;
  let next_fn = scope.alloc_native_function(return_slot0_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next_fn))?;

  let inc_id = vm.register_native_call(inc_host_and_return_number)?;
  let return_name = scope.alloc_string("return")?;
  let return_fn = scope.alloc_native_function(inc_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_fn))?;

  let iterator_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(iterator_obj))?;
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  scope.define_property(iterator_obj, next_key, data_desc(Value::Object(next_fn)))?;
  let return_key = PropertyKey::from_string(scope.alloc_string("return")?);
  scope.define_property(iterator_obj, return_key, data_desc(Value::Object(return_fn)))?;

  let iter_method_name = scope.alloc_string("@@iterator")?;
  let iter_method = scope.alloc_native_function_with_slots(
    return_slot0_id,
    None,
    iter_method_name,
    0,
    &[Value::Object(iterator_obj)],
  )?;
  scope.push_root(Value::Object(iter_method))?;

  let record = iterator::get_iterator_from_method(
    &mut vm,
    &mut host,
    &mut hooks,
    &mut scope,
    Value::Undefined,
    Value::Object(iter_method),
  )?;

  // Throw completion: call `return` but ignore that the result is not an object.
  iterator::iterator_close(
    &mut vm,
    &mut host,
    &mut hooks,
    &mut scope,
    &record,
    CloseCompletionKind::Throw,
  )?;
  assert_eq!(host.count, 1, "expected IteratorClose to call iterator.return");

  // Non-throw completion: call `return` and then throw a TypeError for a non-object result.
  let err = iterator::iterator_close(
    &mut vm,
    &mut host,
    &mut hooks,
    &mut scope,
    &record,
    CloseCompletionKind::NonThrow,
  )
  .expect_err("expected IteratorClose to throw on non-object return value for NonThrow completion");
  assert!(
    matches!(err, VmError::Throw(Value::Object(_))),
    "expected IteratorClose to throw a TypeError object, got {err:?}"
  );
  assert_eq!(host.count, 2, "expected IteratorClose to call iterator.return");

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
