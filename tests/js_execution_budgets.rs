use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{JsExecutionOptions, WindowHost, WindowRealm, WindowRealmConfig};
use fastrender::render_control;
use selectors::context::QuirksMode;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use vm_js::{TerminationReason, VmError};

fn with_interrupt_watchdog<R>(timeout: Duration, f: impl FnOnce() -> R) -> R {
  let interrupt_flag = render_control::interrupt_flag();
  interrupt_flag.store(false, Ordering::Relaxed);

  let (done_tx, done_rx) = mpsc::channel::<()>();
  let flag_for_thread = interrupt_flag.clone();
  let watchdog = std::thread::spawn(move || {
    if done_rx.recv_timeout(timeout).is_err() {
      flag_for_thread.store(true, Ordering::Relaxed);
    }
  });

  let out = f();
  let _ = done_tx.send(());
  let _ = watchdog.join();
  interrupt_flag.store(false, Ordering::Relaxed);
  out
}

#[test]
fn vm_js_infinite_loop_in_classic_script_is_bounded() {
  let mut js_opts = JsExecutionOptions::default();
  js_opts.event_loop_run_limits.max_wall_time = None;
  // Keep this small so termination happens quickly even in debug builds.
  js_opts.max_instruction_count = Some(1_000);

  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts,
  )
  .expect("create realm");

  let err = with_interrupt_watchdog(Duration::from_secs(2), || {
    realm
      .exec_script("while (true) {}")
      .expect_err("expected infinite loop to terminate")
  });

  match err {
    VmError::Termination(term) => assert_eq!(
      term.reason,
      TerminationReason::OutOfFuel,
      "expected OutOfFuel termination, got {term:?}"
    ),
    other => panic!("expected VmError::Termination, got {other:?}"),
  }
}

#[test]
fn vm_js_infinite_loop_in_promise_job_is_bounded() {
  let mut js_opts = JsExecutionOptions::default();
  js_opts.event_loop_run_limits.max_wall_time = None;
  // Small enough to terminate quickly, but large enough to enqueue the Promise job.
  js_opts.max_instruction_count = Some(5_000);

  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_options(dom, "https://example.com/", js_opts)
    .expect("create WindowHost");

  host
    .exec_script("Promise.resolve().then(function () { while (true) {} });")
    .expect("script should complete without throwing");

  let err = with_interrupt_watchdog(Duration::from_secs(2), || {
    host
      .perform_microtask_checkpoint()
      .expect_err("expected Promise job loop to terminate")
  });

  let msg = err.to_string();
  assert!(
    msg.contains("out of fuel") || msg.contains("deadline exceeded"),
    "expected a budget termination error, got: {msg}"
  );
  assert!(
    !msg.contains("interrupted"),
    "expected termination due to budget, but watchdog interrupt fired: {msg}"
  );
}

#[test]
fn vm_js_heap_limit_is_enforced() {
  let mut js_opts = JsExecutionOptions::default();
  js_opts.event_loop_run_limits.max_wall_time = None;
  js_opts.max_instruction_count = Some(100_000);
  js_opts.max_vm_heap_bytes = Some(4 * 1024 * 1024);

  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts,
  )
  .expect("create realm");

  // Allocate an oversized ArrayBuffer in a single step so the test is fast and deterministic even
  // in debug builds (parsing a multi-megabyte string literal can itself dominate runtime).
  let script = "new ArrayBuffer(8 * 1024 * 1024);";
  let err = with_interrupt_watchdog(Duration::from_secs(2), || {
    realm.exec_script(&script).expect_err("expected allocation to OOM")
  });

  match err {
    VmError::OutOfMemory => {}
    VmError::Termination(term) if term.reason == TerminationReason::OutOfMemory => {}
    other => panic!("expected out-of-memory error, got {other:?}"),
  }
}
