use assert_cmd::Command;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

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

fn normalize_report(report: &mut Value) {
  let Some(results) = report.get_mut("results").and_then(|v| v.as_array_mut()) else {
    return;
  };
  for result in results {
    let Some(obj) = result.as_object_mut() else {
      continue;
    };
    obj.insert("duration_ms".to_string(), Value::from(0));
  }
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

#[test]
fn strict_native_cli_matches_built_in_baselines() {
  let report = run_strict_native_json(&[]);
  let total = report["summary"]["total"]
    .as_u64()
    .expect("summary.total must be a number");
  assert!(total >= 10, "expected at least 10 strict-native fixtures");
  assert_eq!(report["summary"]["matched"], report["summary"]["total"]);
  assert_eq!(report["summary"]["mismatched"], 0);
  assert_eq!(report["summary"]["errors"], 0);
  assert_eq!(report["summary"]["updated"], 0);
}

#[test]
fn strict_native_cli_json_is_deterministic() {
  let mut first = run_strict_native_json(&[]);
  let mut second = run_strict_native_json(&[]);
  normalize_report(&mut first);
  normalize_report(&mut second);
  assert_eq!(
    serde_json::to_string(&first).expect("serialize first"),
    serde_json::to_string(&second).expect("serialize second")
  );
}

#[test]
fn strict_native_cli_filter_applies() {
  let report = run_strict_native_json(&["--filter", "any.ts"]);
  assert_eq!(report["summary"]["total"], 1);
  assert_eq!(report["results"].as_array().map(|v| v.len()).unwrap_or(0), 1);
  assert_eq!(report["results"][0]["id"], "any.ts");
}

#[test]
fn strict_native_cli_shards_are_disjoint_and_cover_all_tests() {
  let full = run_strict_native_json(&[]);
  let shard0 = run_strict_native_json(&["--shard", "0/2"]);
  let shard1 = run_strict_native_json(&["--shard", "1/2"]);

  let full_ids: HashSet<_> = ids(&full).into_iter().collect();
  let shard0_ids: HashSet<_> = ids(&shard0).into_iter().collect();
  let shard1_ids: HashSet<_> = ids(&shard1).into_iter().collect();

  assert!(shard0_ids.is_disjoint(&shard1_ids));
  assert_eq!(shard0_ids.union(&shard1_ids).cloned().collect::<HashSet<_>>(), full_ids);
}
