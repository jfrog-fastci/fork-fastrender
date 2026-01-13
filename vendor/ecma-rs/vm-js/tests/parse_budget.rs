use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use vm_js::{Budget, Heap, HeapLimits, JsRuntime, Termination, TerminationReason, Vm, VmError, VmOptions};

fn new_runtime_with_vm(vm: Vm) -> JsRuntime {
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_termination_reason(err: VmError, expected: TerminationReason) -> Termination {
  match err {
    VmError::Termination(term) => {
      assert_eq!(term.reason, expected);
      term
    }
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

fn large_valid_script() -> String {
  // Large enough to trigger at least one periodic parse tick (PARSE_TICK_EVERY=1024).
  ";".repeat(100_000)
}

#[test]
fn exec_script_parsing_consumes_fuel_and_terminates_out_of_fuel() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // 1 tick for parse entry, then OutOfFuel on the first periodic parse tick.
  rt.vm.set_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script(&large_valid_script()).unwrap_err();
  let term = assert_termination_reason(err, TerminationReason::OutOfFuel);

  // Parsing happens before the script frame is pushed; if this fails with a non-empty stack, we're
  // likely terminating later (e.g. at script entry) instead of during parsing.
  assert!(
    term.stack.is_empty(),
    "expected parse-time termination (empty stack), got: {:?}",
    term.stack
  );
}

#[test]
fn exec_script_parsing_respects_deadline_and_terminates_deadline_exceeded() {
  let vm = Vm::new(VmOptions::default());
  let mut rt = new_runtime_with_vm(vm);

  // Use a past deadline but only check time every 2 ticks, so the parse-entry tick succeeds and
  // the deadline is observed on the first periodic parse tick.
  rt.vm.set_budget(Budget {
    fuel: None,
    deadline: Some(Instant::now() - Duration::from_secs(1)),
    check_time_every: 2,
  });

  let err = rt.exec_script(&large_valid_script()).unwrap_err();
  let term = assert_termination_reason(err, TerminationReason::DeadlineExceeded);
  assert!(
    term.stack.is_empty(),
    "expected parse-time termination (empty stack), got: {:?}",
    term.stack
  );
}

#[test]
fn exec_script_parsing_respects_interrupt_flag_and_terminates_interrupted() {
  // Use a shared interrupt flag to simulate host cancellation.
  let interrupt_flag = Arc::new(AtomicBool::new(false));
  let vm = Vm::new(VmOptions {
    interrupt_flag: Some(interrupt_flag.clone()),
    ..VmOptions::default()
  });
  let mut rt = new_runtime_with_vm(vm);

  rt.vm.set_budget(Budget::unlimited(1));

  // Use a small script so the parse entry tick is the only tick during parsing; this catches
  // regressions where the parse entry tick is removed.
  interrupt_flag.store(true, Ordering::Relaxed);
  let err = rt.exec_script("1;").unwrap_err();
  let term = assert_termination_reason(err, TerminationReason::Interrupted);
  assert!(
    term.stack.is_empty(),
    "expected parse-time termination (empty stack), got: {:?}",
    term.stack
  );
}

