use vm_js::{Budget, Heap, HeapLimits, SourceTextModuleRecord, TerminationReason, Vm, VmError, VmOptions};

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn module_record_parse_respects_vm_fuel_budget() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  let err =
    SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, "export const x = 1;").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_parse_is_interruptible_during_record_extraction() {
  // Parsing itself should succeed (only charges one tick for small inputs), but the post-parse
  // record extraction passes (`has_tla` scan + import/export extraction) should also respect fuel.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(2),
    deadline: None,
    check_time_every: 1,
  });

  // Large enough to trigger at least one periodic extraction tick (MODULE_RECORD_TICK_EVERY=256).
  let src = ";".repeat(300);
  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_parse_is_interruptible_during_parsing() {
  // `Vm::parse_top_level_with_budget` charges one tick at parse entry and then periodically during
  // parsing itself. Ensure an out-of-fuel condition observed during parsing is surfaced as VM
  // termination (not as a parser `Cancelled` syntax error).
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  // Large enough to trigger at least one parse tick (PARSE_TICK_EVERY=1024).
  let src = ";".repeat(5000);
  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_top_level_await_scan_budgets_holey_array_literals() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Budget small enough that parsing succeeds but module record extraction (top-level await scan)
    // terminates once the holey array literal is traversed.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // `[, , , ...]` can contain arbitrarily many elisions without any nested expressions. Module
  // record parsing scans expressions to detect top-level `await`; ensure that holey arrays are
  // budgeted so that scan can't do unbounded work within a single tick.
  let mut src = String::from("[");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("];");

  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_top_level_await_scan_budgets_holey_array_patterns() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Array destructuring patterns can also contain elisions (`var [,,,,x] = a`). Ensure that
  // top-level-await scanning budgets traversal even when most pattern elements are holes.
  let mut src = String::from("var [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("x] = a;");
  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_top_level_await_scan_skips_class_field_initializers() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Budget small enough that traversing the large holey array literal would terminate
    // (see `module_record_top_level_await_scan_budgets_holey_array_literals`), but parsing should
    // succeed and `[[HasTLA]]` detection should not descend into class field initializers.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("class C { x = [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("]; } export {};");

  let record = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src)
    .expect("module record parse should not traverse class field initializer");
  assert!(!record.has_tla);
}

#[test]
fn module_record_top_level_await_scan_skips_class_static_blocks() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Class static blocks are `~Await` in modules and cannot contribute to `[[HasTLA]]`. Ensure the
    // scan does not descend into their statement lists (which could contain arbitrarily-large
    // expressions) and therefore does not burn the VM fuel budget.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("class C { static { [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("]; } } export {};");

  let record = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src)
    .expect("module record parse should not traverse class static block bodies");
  assert!(!record.has_tla);
}

#[test]
fn module_record_top_level_await_scan_short_circuits_after_top_level_await() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Ensure the `[[HasTLA]]` scan short-circuits after finding an `await` early in the module so
    // it doesn't need to traverse unrelated large expressions later in the statement list.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("await 0; [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("]; export {};");

  let record = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src)
    .expect("module record parse should short-circuit after finding top-level await");
  assert!(record.has_tla);
}

#[test]
fn module_record_top_level_await_scan_skips_static_import_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    // Budget small enough that traversing the huge holey array would terminate (see
    // `module_record_top_level_await_scan_budgets_holey_array_literals`), but HasTLA detection
    // should not descend into static import attributes at all.
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  // Import attributes are validated later during module record extraction; make the attribute
  // value invalid (non-string) so extraction fails fast without traversing the full array.
  let mut src = String::from("import x from \"m\" with { type: [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("] };");

  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert!(
    matches!(err, VmError::Syntax(_)),
    "expected syntax error for invalid import attributes, got {err:?}"
  );
}

#[test]
fn module_record_top_level_await_scan_skips_static_export_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(50),
    deadline: None,
    check_time_every: 1,
  });

  let mut src = String::from("export * from \"m\" with { type: [");
  for _ in 0..20_000 {
    src.push(',');
  }
  src.push_str("] };");

  let err = SourceTextModuleRecord::parse_with_vm(&mut heap, &mut vm, &src).unwrap_err();
  assert!(
    matches!(err, VmError::Syntax(_)),
    "expected syntax error for invalid export attributes, got {err:?}"
  );
}
