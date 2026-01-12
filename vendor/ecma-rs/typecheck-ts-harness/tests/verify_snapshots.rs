mod common;

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;

const CLI_TIMEOUT: Duration = Duration::from_secs(60);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

#[test]
fn verify_snapshots_succeeds_for_conformance_mini() {
  let node_path = match common::node_path_or_skip("verify-snapshots conformance-mini") {
    Some(path) => path,
    None => return,
  };

  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("fixtures")
    .join("conformance-mini");

  let mut cmd = harness_cli();
  cmd.timeout(CLI_TIMEOUT);
  cmd
    .arg("verify-snapshots")
    .arg("--root")
    .arg(&root)
    .arg("--node")
    .arg(node_path)
    .arg("--jobs")
    .arg("2")
    .arg("--timeout-secs")
    .arg("20")
    .arg("--json");

  let output = cmd.assert().success().get_output().stdout.clone();
  let report: Value = serde_json::from_slice(&output).expect("valid json");

  assert_eq!(report["suite_name"], "conformance-mini");
  assert_eq!(report["summary"]["total"], 6);
  assert_eq!(report["summary"]["ok"], 6);
  assert_eq!(report["summary"]["missing_snapshot"], 0);
  assert_eq!(report["summary"]["drift"], 0);
  assert_eq!(report["summary"]["tsc_crashed"], 0);
  assert_eq!(report["summary"]["timeout"], 0);

  let cases = report["cases"].as_array().expect("cases array");
  assert_eq!(cases.len(), 6);
  assert!(
    cases.iter().all(|case| case["status"] == "ok"),
    "expected all cases to be ok"
  );
}

#[test]
fn verify_snapshots_fails_when_snapshot_is_missing() {
  let node_path = match common::node_path_or_skip("verify-snapshots missing snapshot") {
    Some(path) => path,
    None => return,
  };

  let dir = tempdir().expect("tempdir");
  let root = dir.path().join("conformance-mini");
  fs::create_dir_all(&root).expect("create suite root");
  fs::write(
    root.join("missing.ts"),
    "// @noLib: true\nconst value = 1;\n",
  )
  .expect("write fixture");

  let mut cmd = harness_cli();
  cmd.timeout(CLI_TIMEOUT);
  cmd
    .arg("verify-snapshots")
    .arg("--root")
    .arg(&root)
    .arg("--filter")
    .arg("missing.ts")
    .arg("--node")
    .arg(node_path)
    .arg("--jobs")
    .arg("1")
    .arg("--timeout-secs")
    .arg("20")
    .arg("--json");

  let output = cmd.assert().failure().get_output().stdout.clone();
  let report: Value = serde_json::from_slice(&output).expect("valid json");

  assert_eq!(report["suite_name"], "conformance-mini");
  assert_eq!(report["summary"]["total"], 1);
  assert_eq!(report["summary"]["missing_snapshot"], 1);

  let cases = report["cases"].as_array().expect("cases array");
  assert_eq!(cases.len(), 1);
  assert_eq!(cases[0]["id"], "missing.ts");
  assert_eq!(cases[0]["status"], "missing_snapshot");
  let detail = cases[0]["detail"].as_str().expect("detail string");
  assert!(
    detail.contains("snapshot not found"),
    "expected missing snapshot detail, got {detail:?}"
  );
}

#[test]
fn verify_snapshots_trace_resolution_includes_trace_for_drift() {
  let node_path = match common::node_path_or_skip("verify-snapshots trace-resolution") {
    Some(path) => path,
    None => return,
  };

  let dir = tempdir().expect("tempdir");
  // Name the suite directory `conformance-mini` so the harness picks the committed
  // snapshots under `baselines/conformance-mini/**` for comparison.
  let root = dir.path().join("conformance-mini");
  fs::create_dir_all(root.join("match")).expect("create suite root");

  let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("fixtures")
    .join("conformance-mini")
    .join("match")
    .join("options_passthrough.ts");
  let mut content = fs::read_to_string(&source).expect("read fixture");
  // Introduce a drift vs the committed snapshot.
  content.push_str("\nexport const broken: string = 123;\n");
  fs::write(root.join("match").join("options_passthrough.ts"), content).expect("write fixture");

  let mut cmd = harness_cli();
  cmd.timeout(CLI_TIMEOUT);
  cmd
    .arg("verify-snapshots")
    .arg("--root")
    .arg(&root)
    .arg("--filter")
    .arg("match/options_passthrough.ts")
    .arg("--node")
    .arg(node_path)
    .arg("--jobs")
    .arg("1")
    .arg("--timeout-secs")
    .arg("20")
    .arg("--json")
    .arg("--trace-resolution");

  let output = cmd.assert().failure().get_output().stdout.clone();
  let report: Value = serde_json::from_slice(&output).expect("valid json");

  assert_eq!(report["summary"]["total"], 1);
  assert_eq!(report["summary"]["drift"], 1);
  let cases = report["cases"].as_array().expect("cases array");
  assert_eq!(cases.len(), 1);
  assert_eq!(cases[0]["status"], "drift");
  let trace = cases[0]["tsc_resolution_trace"]
    .as_array()
    .expect("tsc_resolution_trace array");
  assert!(
    !trace.is_empty(),
    "expected tsc_resolution_trace to be non-empty for drifted case"
  );
}
