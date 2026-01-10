#![cfg(feature = "vmjs")]

use js_wpt_dom_runner::{discover_tests, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
      // Virtual timeout must be long enough to allow the test's 1000ms timer to fire.
      default_timeout: Duration::from_secs(2),
      long_timeout: Duration::from_secs(2),
      backend: BackendSelection::VmJs,
      ..RunnerConfig::default()
    },
  );

  // Measure a short baseline test first so we can compare wall-clock overhead. This avoids flakiness
  // from absolute timing thresholds on busy CI machines while still catching accidental real-time
  // sleeps for the long (1000ms) timer.
  let baseline_start = Instant::now();
  let baseline_result = runner.run_test(baseline_test).expect("run baseline test");
  let baseline_elapsed = baseline_start.elapsed();
  assert_eq!(
    baseline_result.outcome,
    RunOutcome::Pass,
    "{baseline_id} should pass under vm-js backend"
  );

  let start = Instant::now();
  let result = runner.run_test(test).expect("run test");
  let elapsed = start.elapsed();

  let extra = elapsed.saturating_sub(baseline_elapsed);
  assert!(
    // The determinism test schedules a 1000ms timer after an initial 10ms timer. With virtual time
    // fast-forward, the additional wall time relative to the baseline should be small. If the
    // backend regresses to real-time sleeps, this delta will approach the timer delay (~1s).
    extra < Duration::from_millis(800),
    "expected long virtual timers to avoid real-time waiting (baseline={baseline_elapsed:?}, determinism={elapsed:?}, extra={extra:?})"
  );
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
