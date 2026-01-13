use vm_js::{
  Agent, Budget, CompiledScript, HeapLimits, HostHooks, Termination, TerminationReason, Value,
  VmError, VmOptions,
};

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

#[test]
fn run_compiled_script_invokes_microtask_checkpoint_hook() {
  let mut agent = Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024),
  )
  .unwrap();

  struct Hooks {
    checkpoints: u32,
  }

  impl HostHooks for Hooks {
    fn microtask_checkpoint(&mut self, agent: &mut Agent) -> Result<(), VmError> {
      self.checkpoints += 1;
      agent.perform_microtask_checkpoint()
    }
  }

  // Enqueue a Promise job that sets a global property. The host hook runs a microtask checkpoint,
  // so the job should execute before `run_compiled_script` returns.
  let src = "Promise.resolve(1).then(() => { globalThis.__x = 123; });";
  let script = CompiledScript::compile_script(agent.heap_mut(), "<regression>", src).unwrap();

  let mut hooks = Hooks { checkpoints: 0 };
  agent
    .run_compiled_script(
      script,
      Budget {
        fuel: Some(10_000),
        deadline: None,
        check_time_every: 1,
      },
      Some(&mut hooks),
    )
    .unwrap();

  assert_eq!(
    hooks.checkpoints, 1,
    "expected microtask checkpoint hook to run"
  );

  let v = agent
    .run_script(
      "<check>",
      "globalThis.__x",
      Budget {
        fuel: Some(10_000),
        deadline: None,
        check_time_every: 1,
      },
      None,
    )
    .unwrap();

  assert_eq!(v, Value::Number(123.0));
}

#[test]
fn run_compiled_script_tears_down_microtasks_on_hook_hard_stop_error() {
  let mut agent = Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024),
  )
  .unwrap();

  struct Hooks;

  impl HostHooks for Hooks {
    fn microtask_checkpoint(&mut self, _agent: &mut Agent) -> Result<(), VmError> {
      // Simulate a hard-stop error in the host checkpoint hook without draining the VM-owned
      // microtask queue.
      Err(VmError::Termination(Termination::new(
        TerminationReason::OutOfFuel,
        Vec::new(),
      )))
    }
  }

  // Enqueue a Promise job (microtask) during script execution.
  let src = "Promise.resolve(1).then(() => { globalThis.__x = 456; });";
  let script = CompiledScript::compile_script(agent.heap_mut(), "<regression>", src).unwrap();

  let mut hooks = Hooks;
  let err = agent
    .run_compiled_script(
      script,
      Budget {
        fuel: Some(10_000),
        deadline: None,
        check_time_every: 1,
      },
      Some(&mut hooks),
    )
    .unwrap_err();

  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  // The job enqueued by the script must be discarded on hard-stop so persistent roots are cleaned
  // up and the embedding can safely reuse the heap.
  assert!(
    agent.vm().microtask_queue().is_empty(),
    "expected VM-owned microtask queue to be torn down on hook termination"
  );
}
