use std::fs;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn ui_perf_smoke_emits_tab_switch_scenario_summary() {
  let temp = tempdir().expect("create temp dir");
  let output = temp.path().join("ui-perf-smoke.json");

  let result = Command::new(env!("CARGO_BIN_EXE_ui_perf_smoke"))
    .args([
      "--output",
      output.to_str().unwrap(),
      "--scenario",
      "tab_switch",
      "--iterations",
      "1",
      "--warmup",
      "0",
    ])
    .stdout(Stdio::null())
    .output()
    .expect("run ui_perf_smoke");

  assert!(
    result.status.success(),
    "ui_perf_smoke should exit successfully; stderr: {}",
    String::from_utf8_lossy(&result.stderr)
  );

  let data = fs::read_to_string(&output).expect("read ui_perf_smoke output");
  let summary: Value = serde_json::from_str(&data).expect("parse ui_perf_smoke json");

  assert_eq!(
    summary["schema_version"].as_u64(),
    Some(1),
    "ui_perf_smoke schema_version should be current"
  );

  assert_eq!(
    summary["run_config"]["warmup"].as_u64(),
    Some(1),
    "run_config.warmup should default to 1"
  );
  assert_eq!(
    summary["run_config"]["isolate"].as_bool(),
    Some(false),
    "run_config.isolate should default to false"
  );
  assert!(
    summary["run_config"]["rayon_threads"].as_u64().is_some(),
    "run_config.rayon_threads should be present"
  );

  let scenarios = summary["scenarios"]
    .as_array()
    .expect("scenarios array must exist");
  assert_eq!(scenarios.len(), 1, "--scenario should filter to one scenario");
  let scenario = &scenarios[0];

  assert_eq!(
    scenario["name"].as_str(),
    Some("tab_switch"),
    "scenario name should match"
  );

  assert!(
    scenario["samples_ms"].as_array().is_some(),
    "scenario should include samples_ms array"
  );
  let metrics = scenario["metrics_ms"]
    .as_object()
    .expect("scenario should include metrics_ms object");
  for key in [
    "tab_switch_latency_p95_ms",
    "tab_switch_latency_max_ms",
    "tab_switch_latency_total_ms",
  ] {
    assert!(
      metrics.get(key).and_then(Value::as_f64).is_some(),
      "scenario metrics_ms should include numeric {key}"
    );
  }
}

#[test]
fn ui_perf_smoke_records_isolate_and_warmup_overrides() {
  let temp = tempdir().expect("create temp dir");
  let output = temp.path().join("ui-perf-smoke.json");

  let result = Command::new(env!("CARGO_BIN_EXE_ui_perf_smoke"))
    .args([
      "--output",
      output.to_str().unwrap(),
      "--warmup",
      "0",
      "--isolate",
      "--only",
      "ttfp_newtab",
      "--no-fail-on-failure",
    ])
    .stdout(Stdio::null())
    .output()
    .expect("run ui_perf_smoke --isolate");

  assert!(
    result.status.success(),
    "ui_perf_smoke --isolate should exit successfully; stderr: {}",
    String::from_utf8_lossy(&result.stderr)
  );

  let data = fs::read_to_string(&output).expect("read ui_perf_smoke output");
  let summary: Value = serde_json::from_str(&data).expect("parse ui_perf_smoke json");
  assert_eq!(
    summary["run_config"]["warmup"].as_u64(),
    Some(0),
    "run_config.warmup should reflect the CLI override"
  );
  assert_eq!(
    summary["run_config"]["isolate"].as_bool(),
    Some(true),
    "run_config.isolate should reflect the CLI override"
  );
}
