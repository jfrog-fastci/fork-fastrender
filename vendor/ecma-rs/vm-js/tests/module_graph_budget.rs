use vm_js::{
  Budget, Heap, HeapLimits, ModuleGraph, Realm, SourceText, SourceTextModuleRecord, TerminationReason, Vm,
  VmError, VmOptions,
};

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn module_graph_link_respects_fuel_budget_even_for_empty_modules() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  let record =
    SourceTextModuleRecord::parse_source(SourceText::new_charged_arc(&mut heap, "m", "")?)?;
  let module = graph.add_module(record)?;

  let err = graph
    .link(&mut vm, &mut heap, realm.global_object(), realm.id(), module)
    .unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn module_graph_namespace_creation_respects_fuel_budget() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Enough fuel for `GetExportedNames`, but not for resolving each export.
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut graph = ModuleGraph::new();
  let record = SourceTextModuleRecord::parse_source(SourceText::new_charged_arc(
    &mut heap,
    "m",
    "export const x = 1;",
  )?)?;
  let module = graph.add_module(record)?;

  {
    let mut scope = heap.scope();
    let err = graph
      .get_module_namespace(module, &mut vm, &mut scope)
      .unwrap_err();
    assert_termination_reason(err, TerminationReason::OutOfFuel);
  }

  realm.teardown(&mut heap);
  Ok(())
}
