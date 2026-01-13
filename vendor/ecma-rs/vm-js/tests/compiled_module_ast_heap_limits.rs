use std::sync::Arc;
use vm_js::{
  Heap, HeapLimits, ModuleGraph, ModuleStatus, RealmId, RootId, SourceText, SourceTextModuleRecord,
  Value, Vm, VmError, VmOptions,
};

#[test]
fn compiled_module_ast_retention_is_charged_and_does_not_leak() -> Result<(), VmError> {
  // Use a small heap limit so charging a retained module AST (estimated at 4x the source length)
  // fails even though storing the source text itself succeeds.
  let max_bytes = 1024 * 1024; // 1 MiB
  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let mut vm = Vm::new(VmOptions::default());

  // Create a dummy global object and keep it alive across GCs: `Heap::charge_external` can trigger
  // collection.
  let (global_object, global_root): (vm_js::GcObject, RootId) = {
    let mut scope = heap.scope();
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    let root = scope.heap_mut().add_root(Value::Object(obj))?;
    (obj, root)
  };
  let realm_id = RealmId::from_raw(0);

  // A large-but-manageable module source. We use `;` so it's syntactically valid without
  // allocating any import/export metadata.
  let src = ";".repeat(400_000);
  let source = Arc::new(SourceText::new_charged(&mut heap, "<inline>", src)?);

  // Simulate a "compiled module" that does not retain an AST by default: populate only the source.
  let mut record = SourceTextModuleRecord::default();
  record.source = Some(source);
  record.status = ModuleStatus::Unlinked;

  let mut graph = ModuleGraph::new();
  let module = graph.add_module(record)?;

  // Attempt linking multiple times. Each attempt should fail due to the retained-AST external
  // memory charge, and should not leak external-memory tokens or persistent roots.
  let mut baseline_total: Option<usize> = None;
  let mut baseline_external: Option<usize> = None;

  for _ in 0..3 {
    // Force the graph to retry the linking path each iteration.
    graph.module_mut(module).status = ModuleStatus::Unlinked;

    let err = graph
      .link(&mut vm, &mut heap, global_object, realm_id, module)
      .unwrap_err();
    assert!(
      matches!(err, VmError::OutOfMemory | VmError::LimitExceeded(_)),
      "expected OOM due to retained AST charging, got {err:?}"
    );

    // Ensure we didn't partially install an uncharged AST.
    assert!(
      graph.module(module).ast.is_none(),
      "module record should not retain an AST when external-memory charging fails"
    );

    let total = heap.estimated_total_bytes();
    let ext = heap.vm_external_bytes();
    match (baseline_total, baseline_external) {
      (Some(prev_total), Some(prev_ext)) => {
        assert_eq!(
          ext, prev_ext,
          "expected Heap::vm_external_bytes to remain stable across failed link attempts"
        );
        assert_eq!(
          total, prev_total,
          "expected Heap::estimated_total_bytes to remain stable across failed link attempts"
        );
      }
      _ => {
        baseline_total = Some(total);
        baseline_external = Some(ext);
      }
    }
  }

  graph.teardown(&mut vm, &mut heap);
  heap.remove_root(global_root);
  Ok(())
}

