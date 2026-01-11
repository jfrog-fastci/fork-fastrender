#![cfg(feature = "vmjs")]

use js_wpt_dom_runner::{discover_tests, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

fn corpus_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/wpt_dom")
    .canonicalize()
    .expect("canonicalize corpus root")
}

fn tests_root() -> PathBuf {
  corpus_root().join("tests")
}

#[test]
fn vmjs_backend_uses_deterministic_virtual_time() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");

  let baseline_id = "vmjs/time_determinism_baseline.window.js";
  let baseline_test = tests
    .iter()
    .find(|t| t.id == baseline_id)
    .unwrap_or_else(|| panic!("missing test {baseline_id}"));

  let id = "vmjs/time_determinism.window.js";
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(
    fs,
    RunnerConfig {
      // Virtual timeout must be long enough to allow the test's long (30s) timer to fire.
      default_timeout: Duration::from_secs(40),
      long_timeout: Duration::from_secs(40),
      backend: BackendSelection::VmJs,
      ..RunnerConfig::default()
    },
  );

  // Run a short baseline test first to warm up one-time initialization paths (DOM bootstrap,
  // intrinsics, etc) so the wall-clock guard below is less sensitive to cold-start variance.
  let baseline_result = runner.run_test(baseline_test).expect("run baseline test");
  assert_eq!(
    baseline_result.outcome,
    RunOutcome::Pass,
    "{baseline_id} should pass under vm-js backend"
  );

  // Ensure the long virtual timer doesn't translate into a long wall-clock delay. We enforce this
  // by running the test in a separate thread with a wall-clock timeout (rather than comparing
  // absolute runtimes, which can be noisy on busy CI machines).
  //
  // If the vm-js backend regresses to real-time sleeps for timers, this test will exceed the
  // timeout and fail quickly (without waiting for the full 30s timer delay).
  const WALL_CLOCK_TIMEOUT: Duration = Duration::from_secs(10);
  let (tx, rx) = mpsc::channel();
  let runner_thread = runner.clone();
  let test_thread = test.clone();
  let handle = std::thread::spawn(move || {
    let result = runner_thread.run_test(&test_thread);
    let _ = tx.send(result);
  });

  let result = match rx.recv_timeout(WALL_CLOCK_TIMEOUT) {
    Ok(r) => r,
    Err(_err) => panic!(
      "expected vm-js backend to fast-forward long virtual timers without real-time waiting (wall_clock_timeout={WALL_CLOCK_TIMEOUT:?})"
    ),
  };
  handle.join().expect("runner thread should not panic");

  let result = result.expect("run test");
  assert_eq!(
    result.outcome,
    RunOutcome::Pass,
    "{id} should pass under vm-js backend"
  );
  let report = result
    .wpt_report
    .unwrap_or_else(|| panic!("{id} should include report payload"));
  assert_eq!(report.file_status, "pass");
  assert_eq!(report.harness_status, "ok");
  assert!(
    !report.subtests.is_empty(),
    "{id} should produce at least one harness subtest"
  );
  for st in &report.subtests {
    assert_eq!(st.status, "pass", "unexpected subtest failure: {st:?}");
  }
}
