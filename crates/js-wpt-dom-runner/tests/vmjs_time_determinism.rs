#![cfg(feature = "vmjs")]

use js_wpt_dom_runner::{discover_tests, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs};
use std::path::PathBuf;
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

  let id = "vmjs/time_determinism.window.js";
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(
    fs,
    RunnerConfig {
      // Tight timeout so a regression back to wall-clock sleeps doesn't stall CI.
      default_timeout: Duration::from_millis(50),
      long_timeout: Duration::from_millis(50),
      backend: BackendSelection::VmJs,
      ..RunnerConfig::default()
    },
  );

  let result = runner.run_test(test).expect("run test");
  assert_eq!(
    result.outcome,
    RunOutcome::Pass,
    "{id} should pass under vm-js backend"
  );
  let report = result
    .wpt_report
    .unwrap_or_else(|| panic!("{id} should include report payload"));
  assert_eq!(report.file_status, "pass");
}

