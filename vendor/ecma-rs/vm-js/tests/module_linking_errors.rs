use vm_js::{
  Heap, HeapLimits, ModuleGraph, PropertyKey, Realm, SourceTextModuleRecord, Value, Vm, VmError,
  VmOptions,
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
fn missing_export_throws_syntax_error_during_link() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier("a", SourceTextModuleRecord::parse("export const x = 1;")?);
  let b = graph.add_module_with_specifier("b", SourceTextModuleRecord::parse("import { y } from \"a\";")?);
  graph.link_all_by_specifier();

  let err = graph
    .link(&mut vm, &mut heap, realm.global_object(), b)
    .expect_err("expected link to throw a SyntaxError");
  let thrown = match err {
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw completion, got {other:?}"),
  };
  assert_error_name(&mut heap, thrown, "SyntaxError")?;

  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn ambiguous_export_throws_syntax_error_during_link() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier("a", SourceTextModuleRecord::parse("export const x = 1;")?);
  graph.add_module_with_specifier("b", SourceTextModuleRecord::parse("export const x = 2;")?);
  graph.add_module_with_specifier(
    "c",
    SourceTextModuleRecord::parse("export * from \"a\"; export * from \"b\";")?,
  );
  let d = graph.add_module_with_specifier("d", SourceTextModuleRecord::parse("import { x } from \"c\";")?);
  graph.link_all_by_specifier();

  let err = graph
    .link(&mut vm, &mut heap, realm.global_object(), d)
    .expect_err("expected link to throw a SyntaxError");
  let thrown = match err {
    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected throw completion, got {other:?}"),
  };
  assert_error_name(&mut heap, thrown, "SyntaxError")?;

  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
