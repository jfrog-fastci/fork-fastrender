use conformance_harness::FailOn;
use js_wpt_dom_runner::{run_suite, BackendSelection, SuiteConfig, TestOutcome};
use std::path::PathBuf;
use std::time::Duration;

fn corpus_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/wpt_dom")
    .canonicalize()
    .expect("canonicalize corpus root")
}

fn backend_quickjs_or_vmjs() -> BackendSelection {
  // Pick an explicit backend so local debugging env vars don't affect test results.
  //
  // Prefer QuickJS when it is available, but allow running the suite smoke tests with
  // `--no-default-features --features vmjs` (i.e. vm-js only).
  if cfg!(feature = "quickjs") {
    BackendSelection::QuickJs
  } else {
    BackendSelection::VmJs
  }
}

#[test]
fn suite_smoke_report_classifies_expected_failures() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("smoke/**".to_string()),
    // Avoid test flakiness from per-test initialization overhead on busy CI machines.
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: backend_quickjs_or_vmjs(),
  })
  .expect("run suite");

  let sync_pass = report
    .results
    .iter()
    .find(|r| r.id == "smoke/sync-pass.html")
    .expect("missing sync-pass.html");
  assert_eq!(
    sync_pass.outcome,
    TestOutcome::Passed,
    "sync-pass.html should pass: {sync_pass:#?}"
  );

  let sync_fail = report
    .results
    .iter()
    .find(|r| r.id == "smoke/sync-fail.html")
    .expect("missing sync-fail.html");
  assert_eq!(sync_fail.outcome, TestOutcome::Failed);
  assert!(
    sync_fail.expected_mismatch,
    "expected xfail should be marked expected_mismatch: {sync_fail:#?}"
  );

  let uncaught_exception = report
    .results
    .iter()
    .find(|r| r.id == "smoke/uncaught-exception.html")
    .expect("missing uncaught-exception.html");
  assert_eq!(uncaught_exception.outcome, TestOutcome::Errored);
  assert!(
    uncaught_exception.expected_mismatch,
    "expected xfail should be marked expected_mismatch: {uncaught_exception:#?}"
  );

  let mismatches = report.summary.mismatches.as_ref().expect("mismatches");
  assert_eq!(mismatches.expected, 3, "expected mismatches");
  let unexpected: Vec<String> = report
    .results
    .iter()
    .filter(|r| r.mismatched && !r.expected_mismatch && !r.flaky)
    .map(|r| format!("{} -> {:?} {:?}", r.id, r.outcome, r.error))
    .collect();
  assert_eq!(
    mismatches.unexpected,
    0,
    "unexpected mismatches: {unexpected:#?}"
  );
  assert_eq!(mismatches.flaky, 0, "flaky mismatches");
}

#[test]
fn suite_event_loop_tests_pass() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("event_loop/**".to_string()),
    // Allow some slack for host overhead; timers are short (0-10ms) but CI can be noisy.
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: backend_quickjs_or_vmjs(),
  })
  .expect("run suite");

  assert_eq!(
    report.summary.total, report.summary.passed,
    "all event_loop tests should pass: {report:#?}"
  );
  assert_eq!(report.summary.failed, 0);
  assert_eq!(report.summary.timed_out, 0);
  assert_eq!(report.summary.errored, 0);
  assert_eq!(report.summary.skipped, 0);
  assert!(
    report.summary.mismatches.is_none(),
    "event_loop suite should have no mismatches: {report:#?}"
  );
}

#[test]
#[cfg(feature = "vmjs")]
fn suite_events_tests_pass() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("events/**".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  assert_eq!(report.summary.failed, 0, "events suite should not fail");
  assert_eq!(report.summary.timed_out, 0, "events suite should not time out");
  assert_eq!(report.summary.errored, 0, "events suite should not error");
  assert_eq!(
    report.summary.total,
    report.summary.passed + report.summary.skipped,
    "events suite should only pass/skip: {report:#?}"
  );
  assert!(
    report.summary.mismatches.is_none(),
    "events suite should have no mismatches: {report:#?}"
  );

  // We intentionally skip the DOM-dependent `events/eventtarget.window.js` test.
  assert_eq!(report.summary.skipped, 1, "expected one skipped test");
}

#[test]
#[cfg(feature = "vmjs")]
fn suite_dom_tests_pass() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("dom/**".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  assert_eq!(report.summary.failed, 0, "dom suite should not fail");
  assert_eq!(report.summary.timed_out, 0, "dom suite should not time out");
  assert_eq!(report.summary.errored, 0, "dom suite should not error");
  assert_eq!(report.summary.skipped, 0, "dom suite should not skip");
  assert_eq!(
    report.summary.total, report.summary.passed,
    "all dom tests should pass: {report:#?}"
  );
  assert!(
    report.summary.mismatches.is_none(),
    "dom suite should have no mismatches: {report:#?}"
  );
}
