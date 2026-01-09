use js_wpt_dom_runner::{Report, TestOutcome, REPORT_SCHEMA_VERSION};
use std::fs;
use std::process::Command;

#[test]
fn js_wpt_dom_cli_runs_single_test_and_writes_report() {
  let dir = tempfile::tempdir().expect("tempdir");
  let report_path = dir.path().join("wpt_dom_report.json");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "js",
      "wpt-dom",
      "--filter",
      "smoke/sync-pass.html",
      "--timeout-secs",
      "5",
      "--long-timeout-secs",
      "5",
      "--fail-on",
      "all",
      "--backend",
      "quickjs",
      "--report",
    ])
    .arg(&report_path)
    .output()
    .expect("run `xtask js wpt-dom`");

  assert!(
    output.status.success(),
    "xtask js wpt-dom should exit successfully\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let json = fs::read_to_string(&report_path).expect("read report");
  let report: Report = serde_json::from_str(&json).expect("parse report");

  assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
  assert_eq!(report.summary.total, 1);
  assert_eq!(report.summary.passed, 1);
  assert_eq!(report.summary.failed, 0);
  assert_eq!(report.summary.timed_out, 0);
  assert_eq!(report.summary.errored, 0);
  assert_eq!(report.summary.skipped, 0);
  assert!(report.summary.mismatches.is_none());

  assert_eq!(report.results.len(), 1);
  let result = &report.results[0];
  assert_eq!(result.id, "smoke/sync-pass.html");
  assert_eq!(result.outcome, TestOutcome::Passed);
  assert!(!result.mismatched);
}
