use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, PromiseState, PropertyKey, Realm,
  SourceTextModuleRecord, Value, Vm, VmError, VmOptions,
};

fn assert_error_name(heap: &mut Heap, value: Value, expected: &str) -> Result<(), VmError> {
  let mut scope = heap.scope();
  scope.push_root(value)?;
  let Value::Object(obj) = value else {
    panic!("expected error object, got {value:?}");
  };

  let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
  let name_value = scope
    .heap()
    .object_get_own_data_property_value(obj, &name_key)?
    .expect("expected own name property");
  let Value::String(name_str) = name_value else {
    panic!("expected error name to be a string, got {name_value:?}");
  };
  assert_eq!(scope.heap().get_string(name_str)?.to_utf8_lossy(), expected);
  Ok(())
}

#[test]
fn evaluate_rejects_with_cached_error_for_errored_modules() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut graph = ModuleGraph::new();
  let m = graph.add_module_with_specifier(
    "m.js",
    SourceTextModuleRecord::parse("export const x = 1; throw 7;")?,
  );
  graph.link_all_by_specifier();
  graph.link(&mut vm, &mut heap, realm.global_object(), m)?;

  let p1 = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut host,
    &mut hooks,
  )?;
  let Value::Object(p1_obj) = p1 else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  {
    let mut scope = heap.scope();
    scope.push_root(p1)?;
    assert_eq!(scope.heap().promise_state(p1_obj)?, PromiseState::Rejected);
    let reason = scope.heap().promise_result(p1_obj)?.unwrap_or(Value::Undefined);
    assert_eq!(reason, Value::Number(7.0));
  }

  // Re-evaluating an errored module must reject deterministically with the cached thrown value.
  let p2 = graph.evaluate(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    m,
    &mut host,
    &mut hooks,
  )?;
  let Value::Object(p2_obj) = p2 else {
    panic!("ModuleGraph::evaluate should return a Promise object");
  };
  {
    let mut scope = heap.scope();
    scope.push_root(p2)?;
    assert_eq!(scope.heap().promise_state(p2_obj)?, PromiseState::Rejected);
    let reason = scope.heap().promise_result(p2_obj)?.unwrap_or(Value::Undefined);
    assert_eq!(reason, Value::Number(7.0));
  }

  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn link_rethrows_cached_error_for_errored_modules() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier("a.js", SourceTextModuleRecord::parse("export const x = 1;")?);
  let b = graph.add_module_with_specifier(
    "b.js",
    SourceTextModuleRecord::parse("import { y } from \"a.js\";")?,
  );
  graph.link_all_by_specifier();

  let err1 = graph
    .link(&mut vm, &mut heap, realm.global_object(), b)
    .expect_err("expected link to throw a SyntaxError");
  let thrown1 = match err1 {
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw completion, got {other:?}"),
  };
  assert_error_name(&mut heap, thrown1, "SyntaxError")?;

  let err2 = graph
    .link(&mut vm, &mut heap, realm.global_object(), b)
    .expect_err("expected cached SyntaxError during second link");
  let thrown2 = match err2 {
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw completion, got {other:?}"),
  };
  assert_error_name(&mut heap, thrown2, "SyntaxError")?;
  assert_eq!(thrown1, thrown2, "second link should rethrow the cached error value");

  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

