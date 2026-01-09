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
    // Use an explicit backend so local debugging env vars don't affect test results.
    backend: BackendSelection::VmJs,
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
