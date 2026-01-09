use js_wpt_dom_runner::{discover_tests, RunOutcome, Runner, RunnerConfig, WptFs};
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

fn assert_wpt_pass(id: &str) {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
  let report = result.wpt_report.expect("missing report payload");
  assert_eq!(report.file_status, "pass");
  assert!(
    !report.subtests.is_empty(),
    "expected non-empty subtests list"
  );
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
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "events/eventtarget.window.js")
    .expect("missing events/eventtarget.window.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn runs_sync_html_smoke_test() {
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
fn runs_promise_html_smoke_test() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/promise-pass.html")
    .expect("missing promise-pass.html");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn runs_settimeout_html_smoke_test() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/async-timeout-pass.html")
    .expect("missing async-timeout-pass.html");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn html_failure_reports_fail_outcome() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/sync-fail.html")
    .expect("missing sync-fail.html");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Fail(_) => {}
    other => panic!("expected Fail, got {other:?}"),
  }
}

#[test]
fn failing_assertions_include_subtest_message_in_report() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/assert_fail.window.js")
    .expect("missing assert_fail.window.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Fail(_) => {}
    other => panic!("expected Fail, got {other:?}"),
  }

  let report = result.wpt_report.expect("missing report payload");
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

#[test]
fn meta_timeout_long_overrides_runner_default_timeout() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/timeout_long.window.js")
    .expect("missing timeout_long.window.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(
    fs,
    RunnerConfig {
      default_timeout: Duration::from_millis(10),
      long_timeout: Duration::from_millis(250),
      ..RunnerConfig::default()
    },
  );
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/unsupported.worker.js")
    .expect("missing unsupported.worker.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Skip(reason) => {
      assert!(reason.contains("worker"), "reason should mention worker");
    }
    other => panic!("expected Skip, got {other:?}"),
  }
}

#[test]
fn discovers_serviceworker_tests_but_skips_them() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/unsupported.serviceworker.js")
    .expect("missing unsupported.serviceworker.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Skip(reason) => {
      assert!(
        reason.contains("service worker"),
        "reason should mention service worker: {reason}"
      );
    }
    other => panic!("expected Skip, got {other:?}"),
  }
}

#[test]
fn discovers_sharedworker_tests_but_skips_them() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/unsupported.sharedworker.js")
    .expect("missing unsupported.sharedworker.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Skip(reason) => {
      assert!(
        reason.contains("shared worker"),
        "reason should mention shared worker: {reason}"
      );
    }
    other => panic!("expected Skip, got {other:?}"),
  }
}
