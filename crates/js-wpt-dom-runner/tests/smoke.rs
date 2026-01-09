use js_wpt_dom_runner::{
  discover_tests, BackendKind, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs,
};
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
    assert!(
      !report.subtests.is_empty(),
      "expected non-empty subtests list"
    );
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
fn queue_microtask_orders_before_timeout() {
  assert_wpt_pass("event_loop/queue_microtask_order.window.js");
}

#[test]
fn settimeout_supports_extra_args() {
  assert_wpt_pass("event_loop/settimeout_args.window.js");
}

#[test]
fn setinterval_can_be_canceled() {
  assert_wpt_pass("event_loop/setinterval_cancel.window.js");
}

#[test]
fn eventtarget_dispatch_order_and_stop_propagation() {
  assert_wpt_pass("events/eventtarget_order.window.js");
}

#[test]
fn passive_listeners_do_not_set_default_prevented() {
  assert_wpt_pass("events/passive_listener.window.js");
}

#[test]
fn runs_eventtarget_window_js_test() {
  assert_wpt_pass("events/eventtarget.window.js");
}

#[test]
fn runs_sync_html_smoke_test() {
  assert_wpt_pass("smoke/sync-pass.html");
}

#[test]
fn runs_promise_html_smoke_test() {
  assert_wpt_pass("smoke/promise-pass.html");
}

#[test]
fn runs_settimeout_html_smoke_test() {
  assert_wpt_pass("smoke/async-timeout-pass.html");
}

#[test]
fn html_failure_reports_fail_outcome() {
  for (backend, result) in run_test_id_all_backends("smoke/sync-fail.html", RunnerConfig::default())
  {
    match result.outcome {
      RunOutcome::Fail(_) => {}
      other => panic!("expected Fail under backend {backend}, got {other:?}"),
    }

    let report = result
      .wpt_report
      .unwrap_or_else(|| panic!("missing report payload under backend {backend}"));
    assert_eq!(report.file_status, "fail");
  }
}

#[test]
fn failing_assertions_include_subtest_message_in_report() {
  for (backend, result) in
    run_test_id_all_backends("smoke/assert_fail.window.js", RunnerConfig::default())
  {
    match result.outcome {
      RunOutcome::Fail(_) => {}
      other => panic!("expected Fail under backend {backend}, got {other:?}"),
    }

    let report = result
      .wpt_report
      .unwrap_or_else(|| panic!("missing report payload under backend {backend}"));
    assert_eq!(report.file_status, "fail");
    let failing = report
      .subtests
      .iter()
      .find(|st| st.status == "fail")
      .expect("expected at least one failing subtest");
    let msg = failing
      .message
      .as_ref()
      .expect("failing subtest should include a message");
    assert!(
      msg.contains("intentional failure"),
      "unexpected failing subtest message: {msg}"
    );
  }
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
fn curated_event_loop_tests() {
  assert_wpt_pass("event_loop/queue_microtask_before_timeout.window.js");
  assert_wpt_pass("event_loop/promise_then_before_timeout.window.js");
}

#[test]
fn curated_eventtarget_tests() {
  assert_wpt_pass("events/eventtarget_dispatch_order.window.js");
}

#[test]
fn element_query_selector_and_query_selector_all_work() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "dom/element_query_selector.window.js")
    .expect("missing element_query_selector.window.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  for (backend, result) in
    run_test_id_all_backends("smoke/unsupported.worker.js", RunnerConfig::default())
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
  for (backend, result) in
    run_test_id_all_backends("smoke/unsupported.serviceworker.js", RunnerConfig::default())
  {
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
  for (backend, result) in
    run_test_id_all_backends("smoke/unsupported.sharedworker.js", RunnerConfig::default())
  {
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
