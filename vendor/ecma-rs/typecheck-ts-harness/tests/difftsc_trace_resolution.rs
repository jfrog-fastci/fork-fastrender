#![cfg(feature = "with-node")]

use assert_cmd::Command;
use serde_json::Value;
use std::time::Duration;

mod common;

const CLI_TIMEOUT: Duration = Duration::from_secs(120);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

#[test]
fn difftsc_trace_resolution_includes_traces_and_is_deterministic() {
  let Some(node_path) = common::node_path_or_skip("difftsc trace-resolution test") else {
    return;
  };

  let (_dir, suite) = common::temp_difftsc_suite(&["trace_resolution_exports"]);

  let run = || {
    harness_cli()
      .timeout(CLI_TIMEOUT)
      .arg("difftsc")
      .arg("--suite")
      .arg(&suite)
      .arg("--node")
      .arg(&node_path)
      .arg("--jobs")
      .arg("1")
      .arg("--compare-rust")
      .arg("--allow-mismatches")
      .arg("--json")
      .arg("--trace-resolution")
      .assert()
      .success()
      .get_output()
      .stdout
      .clone()
  };

  let first = run();
  let second = run();
  assert_eq!(
    first, second,
    "expected `--trace-resolution` JSON output to be deterministic"
  );

  let json: Value = serde_json::from_slice(&first).expect("json output");
  let results = json
    .get("results")
    .and_then(|r| r.as_array())
    .expect("results array");
  assert_eq!(results.len(), 1, "expected a single difftsc fixture result");

  let case = &results[0];
  assert_eq!(
    case.get("name").and_then(|v| v.as_str()),
    Some("trace_resolution_exports")
  );

  let rust_trace = case
    .get("rust_resolution_trace")
    .and_then(|v| v.as_array())
    .expect("rust_resolution_trace array");
  assert!(
    !rust_trace.is_empty(),
    "expected rust_resolution_trace to be non-empty"
  );

  let tsc_trace = case
    .get("tsc_resolution_trace")
    .and_then(|v| v.as_array())
    .expect("tsc_resolution_trace array");
  assert!(
    !tsc_trace.is_empty(),
    "expected tsc_resolution_trace to be non-empty"
  );

  // Verify ordering of the Rust trace is deterministic (sorted by from, specifier, resolved).
  for pair in rust_trace.windows(2) {
    let a = &pair[0];
    let b = &pair[1];
    let a_from = a.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let b_from = b.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let a_spec = a.get("specifier").and_then(|v| v.as_str()).unwrap_or("");
    let b_spec = b.get("specifier").and_then(|v| v.as_str()).unwrap_or("");
    let a_res = a.get("resolved").and_then(|v| v.as_str()).unwrap_or("");
    let b_res = b.get("resolved").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
      (a_from, a_spec, a_res) <= (b_from, b_spec, b_res),
      "expected rust_resolution_trace to be sorted; got {:?} then {:?}",
      (a_from, a_spec, a_res),
      (b_from, b_spec, b_res)
    );
  }
}
