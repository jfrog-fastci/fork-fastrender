use vm_js::{
  Heap, HeapLimits, ModuleGraph, PropertyKey, Realm, SourceTextModuleRecord, Value, Vm, VmError, VmOptions,
};

#[test]
fn module_link_throws_syntax_error_for_unresolvable_indirect_exports() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  graph.add_module_with_specifier(
    "m",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
      export const y = 1;
    "#,
    )?,
  );

  let reexport = graph.add_module_with_specifier(
    "reexport",
    SourceTextModuleRecord::parse(
      &mut heap,
      r#"
      export { x } from "m";
    "#,
    )?,
  );

  graph.link_all_by_specifier();

  let err = graph
    .link(&mut vm, &mut heap, realm.global_object(), realm.id(), reexport)
    .expect_err("expected broken indirect export to throw during linking");

  let thrown = err.thrown_value().expect("expected a thrown value");
  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let mut scope = heap.scope();
  scope.push_root(thrown)?;

  let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
  let name = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &name_key)?
    .expect("expected error object to have a name property");

  let Value::String(name_s) = name else {
    panic!("expected name to be a string, got {name:?}");
  };
  assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "SyntaxError");

  drop(scope);
  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}
