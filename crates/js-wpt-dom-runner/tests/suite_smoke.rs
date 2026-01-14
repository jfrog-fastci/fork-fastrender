#![cfg(any(feature = "vmjs", feature = "quickjs"))]

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
  // Prefer vm-js when it is available, but allow running the suite smoke tests with
  // `--no-default-features --features quickjs` (i.e. QuickJS only).
  if cfg!(feature = "vmjs") {
    BackendSelection::VmJs
  } else {
    BackendSelection::QuickJs
  }
}

#[test]
fn suite_smoke_report_classifies_expected_failures() {
  let corpus_root = corpus_root();

  let backend = backend_quickjs_or_vmjs();
  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("smoke/**".to_string()),
    // Avoid test flakiness from per-test initialization overhead on busy CI machines.
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend,
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

  let dom_shims = report
    .results
    .iter()
    .find(|r| r.id == "smoke/dom_shims.window.js")
    .expect("missing dom_shims.window.js");
  assert_eq!(
    dom_shims.outcome,
    TestOutcome::Passed,
    "dom_shims.window.js should pass: {dom_shims:#?}"
  );

  let infinite_loop = report
    .results
    .iter()
    .find(|r| r.id == "smoke/infinite_loop_timeout.window.js")
    .expect("missing infinite_loop_timeout.window.js");
  assert_eq!(infinite_loop.outcome, TestOutcome::TimedOut);
  assert!(
    infinite_loop.expected_mismatch,
    "expected xfail should be marked expected_mismatch: {infinite_loop:#?}"
  );

  let infinite_loop_timeout = report
    .results
    .iter()
    .find(|r| r.id == "smoke/infinite_loop_timeout.window.js")
    .expect("missing infinite_loop_timeout.window.js");
  assert_eq!(
    infinite_loop_timeout.outcome,
    TestOutcome::TimedOut,
    "infinite_loop_timeout.window.js should time out under {backend:?}: {infinite_loop_timeout:#?}"
  );
  assert!(
    infinite_loop_timeout.expected_mismatch,
    "expected xfail should be marked expected_mismatch: {infinite_loop_timeout:#?}"
  );

  let mismatches = report.summary.mismatches.as_ref().expect("mismatches");
  assert_eq!(mismatches.expected, 7, "expected mismatches");
  let unexpected: Vec<String> = report
    .results
    .iter()
    .filter(|r| r.mismatched && !r.expected_mismatch && !r.flaky)
    .map(|r| format!("{} -> {:?} {:?}", r.id, r.outcome, r.error))
    .collect();
  assert_eq!(
    mismatches.unexpected, 0,
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
fn suite_url_tests_pass() {
  let corpus_root = corpus_root();

  let backend = backend_quickjs_or_vmjs();
  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("url/**".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend,
  })
  .expect("run suite");

  assert!(
    report.summary.total > 0,
    "expected at least one URL test result"
  );
  assert_eq!(report.summary.failed, 0);
  assert_eq!(report.summary.timed_out, 0);
  assert_eq!(report.summary.errored, 0);
  assert!(
    report.summary.mismatches.is_none(),
    "url suite should have no mismatches: {report:#?}"
  );
  assert_eq!(
    report.summary.skipped, 0,
    "url/** tests should run (not skip) under {backend:?}: {report:#?}"
  );
  assert_eq!(
    report.summary.total, report.summary.passed,
    "all url/** tests should pass under {backend:?}: {report:#?}"
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
  assert_eq!(
    report.summary.timed_out, 0,
    "events suite should not time out"
  );
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
  assert_eq!(report.summary.skipped, 0, "events suite should not skip");
  assert_eq!(
    report.summary.total, report.summary.passed,
    "all events tests should pass: {report:#?}"
  );

  for result in &report.results {
    let Some(wpt_report) = &result.wpt_report else {
      panic!("missing WptReport payload for {}", result.id);
    };
    assert!(
      !wpt_report.subtests.is_empty(),
      "expected {} to report at least one subtest: {wpt_report:#?}",
      result.id
    );
  }
}

#[test]
#[cfg(feature = "vmjs")]
fn suite_dom_tests_pass() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("dom/**,domparsing/**".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  assert_eq!(
    report.summary.failed, 0,
    "dom/domparsing suite should not fail"
  );
  assert_eq!(
    report.summary.timed_out, 0,
    "dom/domparsing suite should not time out"
  );
  assert_eq!(
    report.summary.errored, 0,
    "dom/domparsing suite should not error"
  );
  assert_eq!(
    report.summary.skipped, 0,
    "dom/domparsing suite should not skip"
  );
  assert_eq!(
    report.summary.total, report.summary.passed,
    "all dom/domparsing tests should pass: {report:#?}"
  );
  assert!(
    report.summary.mismatches.is_none(),
    "dom/domparsing suite should have no mismatches: {report:#?}"
  );
}

#[test]
#[cfg(feature = "vmjs")]
fn suite_modules_tests_pass() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("modules/**".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  assert_eq!(report.summary.failed, 0, "modules suite should not fail");
  assert_eq!(report.summary.timed_out, 0, "modules suite should not time out");
  assert_eq!(report.summary.errored, 0, "modules suite should not error");
  assert_eq!(report.summary.skipped, 0, "modules suite should not skip");
  assert_eq!(
    report.summary.total, report.summary.passed,
    "all modules tests should pass: {report:#?}"
  );
  assert!(
    report.summary.mismatches.is_none(),
    "modules suite should have no mismatches: {report:#?}"
  );
}

#[test]
#[cfg(feature = "vmjs")]
fn suite_filter_supports_comma_separated_globs() {
  let corpus_root = corpus_root();

  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some("dom/element_matches_closest.window.js,events/eventtarget.window.js".to_string()),
    timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  assert_eq!(
    report.summary.total, 2,
    "expected filter to select exactly two tests: {report:#?}"
  );
  assert_eq!(
    report.summary.passed, 2,
    "expected selected tests to pass: {report:#?}"
  );
  assert!(
    report.summary.mismatches.is_none(),
    "expected no mismatches: {report:#?}"
  );
}
