use assert_cmd::Command;
use serde_json::Value;
use std::time::Duration;

mod common;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

#[test]
fn difftsc_module_resolution_computed_defaults_match_baseline() {
  let (_dir, suite) = common::temp_difftsc_suite(&["module_resolution_default_bundler"]);

  let output = harness_cli()
    .timeout(CLI_TIMEOUT)
    .arg("difftsc")
    .arg("--suite")
    .arg(&suite)
    .arg("--jobs")
    .arg("1")
    .arg("--use-baselines")
    .arg("--compare-rust")
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("json output");
  let results = json
    .get("results")
    .and_then(|r| r.as_array())
    .expect("results array");

  let case = results
    .iter()
    .find(|case| {
      case.get("name").and_then(|n| n.as_str()) == Some("module_resolution_default_bundler")
    })
    .expect("module_resolution_default_bundler case present");
  assert_eq!(
    case.get("status").and_then(|s| s.as_str()),
    Some("matched"),
    "expected computed moduleResolution defaults to match baseline; case={case:?}"
  );

  let tsc = case
    .get("tsc_options")
    .and_then(|o| o.as_object())
    .expect("tsc_options present");
  assert_eq!(
    tsc.get("moduleResolution").and_then(|v| v.as_str()),
    Some("bundler"),
    "expected moduleResolution=bundler computed default to be passed to tsc; case={case:?}"
  );
  assert_eq!(
    tsc.get("module").and_then(|v| v.as_str()),
    Some("ESNext"),
    "expected module=ESNext to be passed to tsc; case={case:?}"
  );
  assert_eq!(
    tsc.get("moduleDetection").and_then(|v| v.as_str()),
    Some("auto"),
    "expected moduleDetection=auto computed default to be passed to tsc; case={case:?}"
  );
}
