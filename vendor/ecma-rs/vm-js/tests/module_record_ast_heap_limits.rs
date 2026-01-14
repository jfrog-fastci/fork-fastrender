use vm_js::{Heap, HeapLimits, SourceText, SourceTextModuleRecord, Vm, VmError, VmOptions};

fn assert_oom_or_limit(err: VmError) {
  assert!(
    matches!(err, VmError::OutOfMemory | VmError::LimitExceeded(_)),
    "expected OutOfMemory/LimitExceeded due to AST external-memory charging, got {err:?}"
  );
}

#[test]
fn parse_source_charges_retained_ast_and_does_not_leak() -> Result<(), VmError> {
  // Use a small heap limit so charging a retained module AST (estimated at 4x the source length)
  // fails even though storing the source text itself succeeds.
  let max_bytes = 1024 * 1024; // 1 MiB
  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));

  // A large-but-manageable module source. We use `;` so it's syntactically valid without
  // allocating any import/export metadata.
  let src = ";".repeat(400_000);
  let source = SourceText::new_charged_arc(&mut heap, "<inline>", src)?;

  let mut baseline_total: Option<usize> = None;
  let mut baseline_external: Option<usize> = None;

  for _ in 0..3 {
    let err = SourceTextModuleRecord::parse_source(&mut heap, source.clone()).unwrap_err();
    assert_oom_or_limit(err);

    let total = heap.estimated_total_bytes();
    let ext = heap.vm_external_bytes();
    match (baseline_total, baseline_external) {
      (Some(prev_total), Some(prev_ext)) => {
        assert_eq!(
          ext, prev_ext,
          "expected Heap::vm_external_bytes to remain stable across failed parse attempts"
        );
        assert_eq!(
          total, prev_total,
          "expected Heap::estimated_total_bytes to remain stable across failed parse attempts"
        );
      }
      _ => {
        baseline_total = Some(total);
        baseline_external = Some(ext);
      }
    }
  }

  Ok(())
}

#[test]
fn parse_source_with_vm_charges_retained_ast_and_does_not_leak() -> Result<(), VmError> {
  let max_bytes = 1024 * 1024; // 1 MiB
  let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let mut vm = Vm::new(VmOptions::default());

  let src = ";".repeat(400_000);
  let source = SourceText::new_charged_arc(&mut heap, "<inline>", src)?;

  let mut baseline_total: Option<usize> = None;
  let mut baseline_external: Option<usize> = None;

  for _ in 0..3 {
    let err =
      SourceTextModuleRecord::parse_source_with_vm(&mut vm, &mut heap, source.clone()).unwrap_err();
    assert_oom_or_limit(err);

    let total = heap.estimated_total_bytes();
    let ext = heap.vm_external_bytes();
    match (baseline_total, baseline_external) {
      (Some(prev_total), Some(prev_ext)) => {
        assert_eq!(
          ext, prev_ext,
          "expected Heap::vm_external_bytes to remain stable across failed parse attempts"
        );
        assert_eq!(
          total, prev_total,
          "expected Heap::estimated_total_bytes to remain stable across failed parse attempts"
        );
      }
      _ => {
        baseline_total = Some(total);
        baseline_external = Some(ext);
      }
    }
  }

  Ok(())
}
