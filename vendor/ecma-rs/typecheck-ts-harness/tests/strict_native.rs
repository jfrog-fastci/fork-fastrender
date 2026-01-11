use assert_cmd::Command;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use typecheck_ts_harness::{build_filter, discover_conformance_tests, Filter, Shard};

const CLI_TIMEOUT: Duration = Duration::from_secs(60);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

fn run_strict_native_json(extra_args: &[&str]) -> Value {
  let mut cmd = harness_cli();
  cmd.timeout(CLI_TIMEOUT);
  cmd.arg("strict-native").arg("--json");
  for arg in extra_args {
    cmd.arg(arg);
  }
  let output = cmd.assert().success().get_output().stdout.clone();
  serde_json::from_slice(&output).expect("valid strict-native json")
}

fn ids(report: &Value) -> Vec<String> {
  let Some(results) = report.get("results").and_then(|v| v.as_array()) else {
    return Vec::new();
  };
  results
    .iter()
    .filter_map(|result| result.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
    .collect()
}

fn fixtures_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/strict-native")
}

#[test]
fn strict_native_fixture_set_has_baselines() {
  let root = fixtures_root();
  let cases = discover_conformance_tests(&root, &Filter::All, &vec!["ts".to_string()])
    .expect("discover strict-native fixture set");
  assert!(
    !cases.is_empty(),
    "expected strict-native fixture set to contain at least one test"
  );

  let baselines_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines/strict-native");
  for case in cases {
    let baseline_path = baseline_path_for(&baselines_root, &case.id);
    assert!(
      baseline_path.exists(),
      "missing baseline for {} at {}",
      case.id,
      baseline_path.display()
    );
  }
}

fn baseline_path_for(baselines_root: &Path, id: &str) -> PathBuf {
  let rel = Path::new(id);
  let mut path = baselines_root.join(rel);
  if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
    path.set_file_name(format!("{name}.json"));
  } else {
    path.set_file_name("baseline.json");
  }
  path
}

#[test]
fn strict_native_cli_matches_built_in_baselines() {
  // Run a small representative subset of fixtures to keep integration tests fast
  // while still exercising the CLI, baseline parsing, and comparison logic.
  let report = run_strict_native_json(&["--filter", "{any.ts,as_const_ok.ts,global_eval.ts}"]);
  assert_eq!(report["summary"]["total"], 3);
  assert_eq!(report["summary"]["matched"], report["summary"]["total"]);
  assert_eq!(report["summary"]["mismatched"], 0);
  assert_eq!(report["summary"]["errors"], 0);
  assert_eq!(report["summary"]["updated"], 0);

  let ids = ids(&report);
  assert_eq!(ids.len(), 3, "summary.total should match results length");
  assert!(
    ids.windows(2).all(|w| w[0] <= w[1]),
    "results should be sorted by id"
  );
}

#[test]
fn strict_native_cli_shard_matches_sorted_index_strategy() {
  let root = fixtures_root();
  let filter = build_filter(Some("computed_*")).expect("build computed_* filter");
  let cases = discover_conformance_tests(&root, &filter, &vec!["ts".to_string()])
    .expect("discover computed_* strict-native fixtures");
  assert!(
    cases.len() >= 2,
    "expected at least two computed_* strict-native fixtures for sharding test"
  );

  let shard = Shard::parse("0/2").expect("parse shard spec");
  let expected_ids: Vec<_> = cases
    .iter()
    .enumerate()
    .filter(|(idx, _)| shard.includes(*idx))
    .map(|(_, case)| case.id.clone())
    .collect();

  let shard0 = run_strict_native_json(&["--filter", "computed_*", "--shard", "0/2"]);
  let shard0_ids = ids(&shard0);
  assert_eq!(shard0_ids, expected_ids);
}
