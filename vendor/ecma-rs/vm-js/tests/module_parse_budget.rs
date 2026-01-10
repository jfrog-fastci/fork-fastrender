use vm_js::{Budget, SourceTextModuleRecord, TerminationReason, Vm, VmError, VmOptions};

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn module_record_parse_respects_vm_fuel_budget() {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(0),
    deadline: None,
    check_time_every: 1,
  });

  let err = SourceTextModuleRecord::parse_with_vm(&mut vm, "export const x = 1;").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_parse_is_interruptible_during_record_extraction() {
  // Parsing itself should succeed (only charges one tick for small inputs), but the post-parse
  // record extraction passes (`has_tla` scan + import/export extraction) should also respect fuel.
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(2),
    deadline: None,
    check_time_every: 1,
  });

  // Large enough to trigger at least one periodic extraction tick (MODULE_RECORD_TICK_EVERY=256).
  let src = ";".repeat(300);
  let err = SourceTextModuleRecord::parse_with_vm(&mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}

#[test]
fn module_record_parse_is_interruptible_during_parsing() {
  // `Vm::parse_top_level_with_budget` charges one tick at parse entry and then periodically during
  // parsing itself. Ensure an out-of-fuel condition observed during parsing is surfaced as VM
  // termination (not as a parser `Cancelled` syntax error).
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  // Large enough to trigger at least one parse tick (PARSE_TICK_EVERY=1024).
  let src = ";".repeat(5000);
  let err = SourceTextModuleRecord::parse_with_vm(&mut vm, &src).unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
}
