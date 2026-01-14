use crate::{
  Heap, HeapLimits, MicrotaskQueue, ModuleGraph, Realm, SourceText, SourceTextModuleRecord, Vm, VmError,
  VmOptions,
};

#[test]
fn module_ast_is_cleared_after_successful_evaluation_even_without_token() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let mut hooks = MicrotaskQueue::new();
  let mut host = ();

  let mut record = SourceTextModuleRecord::parse(&mut heap, "export const x = 1;")?;
  assert!(record.ast.is_some());

  // Simulate a host-provided AST without an external-memory token (the pre-fix behavior of
  // `parse_source`). ModuleGraph should still clear ASTs once a module is complete so long-lived
  // graphs don't retain large parse trees.
  record.ast_external_memory = None;

  let mut graph = ModuleGraph::new();
  let module = graph.add_module(record)?;

  assert!(graph.module(module).ast.is_some());
  graph.evaluate_sync(
    &mut vm,
    &mut heap,
    realm.global_object(),
    realm.id(),
    module,
    &mut host,
    &mut hooks,
  )?;

  assert!(
    graph.module(module).ast.is_none(),
    "module AST should be cleared after evaluation"
  );

  graph.teardown(&mut vm, &mut heap);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn parse_source_rejects_large_modules_when_ast_charge_exceeds_heap_limits() {
  // Keep the heap limits small enough that the SourceText itself fits, but the retained AST estimate
  // (`len * 4`) does not.
  let mut heap = Heap::new(HeapLimits::new(16 * 1024, 16 * 1024));

  let padding = "x".repeat(6 * 1024);
  let src = format!("export default 0;/*{padding}*/");
  let source = SourceText::new_charged_arc(&mut heap, "m.js", src).expect("source text should fit");

  let err = SourceTextModuleRecord::parse_source(&mut heap, source).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}

