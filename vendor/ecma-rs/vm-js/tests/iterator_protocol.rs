use vm_js::{iterator, Heap, HeapLimits, JsRuntime, MicrotaskQueue, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn get_iterator_protocol_over_array_returns_iterator_result_objects() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let array = rt.exec_script("[1,2,3]").unwrap();

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut scope = heap.scope();

  // Root the array across iterator acquisition to keep it alive if allocations trigger GC.
  scope.push_root(array)?;

  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let record = iterator::get_iterator_protocol(vm, &mut host, &mut hooks, &mut scope, array)?;

  // `get_iterator_protocol` must use the iterator protocol (no Array fast-path), so `next_method`
  // must be callable and the iterator object should be distinct from the array itself.
  assert!(scope.heap().is_callable(record.next_method)?);
  let Value::Object(array_obj) = array else {
    panic!("expected array object, got {array:?}");
  };
  let Value::Object(iter_obj) = record.iterator else {
    panic!("expected iterator object, got {:?}", record.iterator);
  };
  assert_ne!(
    array_obj, iter_obj,
    "protocol iterator acquisition should create an iterator object"
  );

  // Root the iterator object across `next()` calls.
  scope.push_root(record.iterator)?;

  let r1 = iterator::iterator_next(vm, &mut host, &mut hooks, &mut scope, &record)?;
  scope.push_root(r1)?;
  assert!(!iterator::iterator_complete(vm, &mut host, &mut hooks, &mut scope, r1)?);
  assert_eq!(
    iterator::iterator_value(vm, &mut host, &mut hooks, &mut scope, r1)?,
    Value::Number(1.0)
  );

  let r2 = iterator::iterator_next(vm, &mut host, &mut hooks, &mut scope, &record)?;
  scope.push_root(r2)?;
  assert!(!iterator::iterator_complete(vm, &mut host, &mut hooks, &mut scope, r2)?);
  assert_eq!(
    iterator::iterator_value(vm, &mut host, &mut hooks, &mut scope, r2)?,
    Value::Number(2.0)
  );

  let r3 = iterator::iterator_next(vm, &mut host, &mut hooks, &mut scope, &record)?;
  scope.push_root(r3)?;
  assert!(!iterator::iterator_complete(vm, &mut host, &mut hooks, &mut scope, r3)?);
  assert_eq!(
    iterator::iterator_value(vm, &mut host, &mut hooks, &mut scope, r3)?,
    Value::Number(3.0)
  );

  let r4 = iterator::iterator_next(vm, &mut host, &mut hooks, &mut scope, &record)?;
  scope.push_root(r4)?;
  assert!(iterator::iterator_complete(vm, &mut host, &mut hooks, &mut scope, r4)?);
  assert_eq!(
    iterator::iterator_value(vm, &mut host, &mut hooks, &mut scope, r4)?,
    Value::Undefined
  );

  Ok(())
}

