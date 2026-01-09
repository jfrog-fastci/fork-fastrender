use conformance_harness::FailOn;
use js_wpt_dom_runner::{run_suite, SuiteConfig, TestOutcome};
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
    timeout: Duration::from_millis(100),
    long_timeout: Duration::from_millis(500),
    fail_on: FailOn::New,
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

  let mismatches = report.summary.mismatches.as_ref().expect("mismatches");
  assert_eq!(mismatches.expected, 1, "expected mismatches");
  assert_eq!(mismatches.unexpected, 1, "unexpected mismatches");
  assert_eq!(mismatches.flaky, 0, "flaky mismatches");
}
