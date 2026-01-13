use vm_js::{Agent, Budget, CompiledScript, HeapLimits, TerminationReason, VmError, VmOptions};

#[test]
fn regression_infinite_loop_is_bounded_in_agent_run_script() {
  let mut agent = Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024),
  )
  .unwrap();

  // This seed is tracked in-tree so a fuzz-found hang regression stays fixed even if local
  // `cargo fuzz` corpora/artifacts are pruned.
  let src = include_str!("../fuzz/corpus/vm_js_exec/infinite_while.js");

  let err = agent
    .run_script(
      "<regression>",
      src,
      Budget {
        fuel: Some(100),
        deadline: None,
        check_time_every: 1,
      },
      None,
    )
    .unwrap_err();

  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }
}

#[test]
fn regression_infinite_loop_is_bounded_in_agent_run_compiled_script() {
  let mut agent = Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024),
  )
  .unwrap();

  // This seed is tracked in-tree so a fuzz-found hang regression stays fixed even if local
  // `cargo fuzz` corpora/artifacts are pruned.
  let src = include_str!("../fuzz/corpus/vm_js_exec/infinite_while.js");

  let script = CompiledScript::compile_script(agent.heap_mut(), "<regression>", src).unwrap();

  let err = agent
    .run_compiled_script(
      script,
      Budget {
        fuel: Some(100),
        deadline: None,
        check_time_every: 1,
      },
      None,
    )
    .unwrap_err();

  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }
}
