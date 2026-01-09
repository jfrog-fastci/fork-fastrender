use js_wpt_dom_runner::{discover_tests, BackendKind, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs};
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

fn run_test_id_all_backends(
  id: &str,
  config: RunnerConfig,
) -> Vec<(BackendKind, js_wpt_dom_runner::RunResult)> {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let mut out = Vec::new();
  for backend in BackendKind::all_available() {
    let mut config = config.clone();
    config.backend = match backend {
      BackendKind::QuickJs => BackendSelection::QuickJs,
      BackendKind::VmJs => BackendSelection::VmJs,
    };

    let fs = WptFs::new(&corpus_root).expect("wpt fs");
    let runner = Runner::new(fs, config);
    let result = runner.run_test(test).expect("run test");
    out.push((backend, result));
  }
  out
}

fn assert_wpt_pass(id: &str) {
  for (backend, result) in run_test_id_all_backends(id, RunnerConfig::default()) {
    assert_eq!(
      result.outcome,
      RunOutcome::Pass,
      "{id} should pass under backend {backend}"
    );
    let report = result
      .wpt_report
      .unwrap_or_else(|| panic!("{id} should include report payload under backend {backend}"));
    assert_eq!(report.file_status, "pass");
  }
}

#[test]
fn runs_window_js_smoke_test() {
  assert_wpt_pass("smoke/meta_script.window.js");
}

#[test]
fn runs_any_js_in_window_realm() {
  assert_wpt_pass("smoke/any_promise.any.js");
}

#[test]
fn runs_html_smoke_test() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/sync-pass.html")
    .expect("missing sync-pass.html");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn runs_setinterval_and_clearinterval() {
  assert_wpt_pass("smoke/interval_cancel.window.js");
}

#[test]
fn meta_timeout_long_overrides_runner_default_timeout() {
  for (backend, result) in run_test_id_all_backends(
    "smoke/timeout_long.window.js",
    RunnerConfig {
      default_timeout: Duration::from_millis(10),
      long_timeout: Duration::from_millis(250),
      ..RunnerConfig::default()
    },
  ) {
    assert_eq!(
      result.outcome,
      RunOutcome::Pass,
      "timeout_long should pass under backend {backend}"
    );
  }
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  for (backend, result) in run_test_id_all_backends("smoke/unsupported.worker.js", RunnerConfig::default())
  {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("worker"),
          "reason should mention worker under backend {backend}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}

#[test]
fn discovers_serviceworker_tests_but_skips_them() {
  for (backend, result) in run_test_id_all_backends(
    "smoke/unsupported.serviceworker.js",
    RunnerConfig::default(),
  ) {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("service worker"),
          "reason should mention service worker under backend {backend}: {reason}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}

#[test]
fn discovers_sharedworker_tests_but_skips_them() {
  for (backend, result) in run_test_id_all_backends(
    "smoke/unsupported.sharedworker.js",
    RunnerConfig::default(),
  ) {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("shared worker"),
          "reason should mention shared worker under backend {backend}: {reason}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}
