use vm_js::{Budget, Heap, HeapLimits, Job, JobKind, JsRuntime, RealmId, RootId, Value, Vm, VmError, VmHostHooks, VmJobContext, VmOptions};

struct DummyJobContext {
  heap: Heap,
}

impl DummyJobContext {
  fn new() -> Self {
    Self {
      heap: Heap::new(HeapLimits::new(1024 * 1024, 512 * 1024)),
    }
  }
}

impl VmJobContext for DummyJobContext {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("DummyJobContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("DummyJobContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

struct DummyHostHooks;

impl VmHostHooks for DummyHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}

#[test]
fn job_run_converts_panics_to_errors() {
  let mut ctx = DummyJobContext::new();
  let mut host = DummyHostHooks;

  let job = Job::new(JobKind::Promise, |_ctx, _host| -> Result<(), VmError> {
    panic!("boom");
  });

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run(&mut ctx, &mut host)));
  assert!(result.is_ok(), "Job::run must not panic");

  let err = result.unwrap().unwrap_err();
  assert!(matches!(err, VmError::InvariantViolation("job closure panicked")));
}

#[test]
fn exec_script_never_panics_on_adversarial_inputs() -> Result<(), VmError> {
  let mut opts = VmOptions::default();
  // Ensure scripts that loop terminate quickly and deterministically for this test.
  opts.default_fuel = Some(10_000);
  opts.check_time_every = 1;

  let vm = Vm::new(opts);
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // A small corpus of "nasty" scripts. Some are invalid syntax; others terminate via budget.
  // The goal is strictly that none of them cause a Rust panic in the engine.
  let mut scripts: Vec<String> = vec![
    "".into(),
    "1 + 2".into(),
    "({a:1, b:2}).a".into(),
    // Parser error.
    "function(".into(),
    // Promise/microtask creation.
    "Promise.resolve(1).then(x => x + 1);".into(),
    // BigInt fast paths / conversions.
    "1n << 3n".into(),
    "1n >> -2n".into(),
    "1n & 2n".into(),
    // Builtins: Array.prototype.reverse holes / property access.
    "let a = [1,,3]; a.reverse();".into(),
    // Terminate via fuel: infinite loop.
    "while (true) {}".into(),
  ];

  // Fuzz-like random small programs (mostly syntax errors). Deterministic seed for reproducibility.
  let alphabet: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789+-*/%(){}[];,.<>=!&|^?:'\" \n\t";
  let mut rng: u64 = 0x6d5a_56da_4d83_2e31;
  for _ in 0..128usize {
    // Xorshift64*
    rng ^= rng >> 12;
    rng ^= rng << 25;
    rng ^= rng >> 27;
    let r = rng.wrapping_mul(0x2545F4914F6CDD1D);

    let len = (r as usize) % 64;
    let mut s = String::new();
    s.try_reserve(len).map_err(|_| VmError::OutOfMemory)?;
    for i in 0..len {
      let idx = (r.wrapping_add(i as u64) as usize) % alphabet.len();
      s.push(alphabet[idx] as char);
    }
    scripts.push(s);
  }

  for (idx, source) in scripts.iter().enumerate() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      rt.vm.reset_budget_to_default();
      // Ensure any script that loops terminates within a bounded number of ticks.
      rt.vm.set_budget(Budget {
        fuel: Some(10_000),
        deadline: None,
        check_time_every: 1,
      });
      let result = rt.exec_script(source);

      // `exec_script` can enqueue Promise jobs onto the VM-owned microtask queue. Those jobs can own
      // persistent roots, so we must explicitly tear down the queue to avoid leaking roots (and
      // triggering debug assertions) when the runtime is dropped at the end of the test.
      let mut queue = std::mem::take(rt.vm.microtask_queue_mut());
      queue.cancel_all(&mut rt);

      result
    }));

    assert!(
      result.is_ok(),
      "exec_script panicked on corpus entry {idx}: {source:?}"
    );
    // Ignore the Result itself: syntax errors / terminations are fine.
    let _ = result.unwrap();
  }

  Ok(())
}
