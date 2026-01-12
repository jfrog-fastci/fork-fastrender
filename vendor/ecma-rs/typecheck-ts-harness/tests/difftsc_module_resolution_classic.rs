use assert_cmd::Command;
use serde_json::Value;
use std::time::Duration;

mod common;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

#[test]
fn difftsc_module_resolution_classic_vs_node_matches_baselines() {
  let (_dir, suite) =
    common::temp_difftsc_suite(&["module_resolution_classic", "module_resolution_node"]);

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

  for name in ["module_resolution_classic", "module_resolution_node"] {
    let case = results
      .iter()
      .find(|case| case.get("name").and_then(|n| n.as_str()) == Some(name))
      .unwrap_or_else(|| panic!("{name} case present"));
    assert_eq!(
      case.get("status").and_then(|s| s.as_str()),
      Some("matched"),
      "{name} should match tsc baseline; case={case:?}"
    );
  }
}

