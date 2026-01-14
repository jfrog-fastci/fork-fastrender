use serde_json::Value;
use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn ui_perf_smoke_denies_network_by_default() {
  let temp = tempdir().expect("temp dir");
  let output_path = temp.path().join("ui_perf_smoke.json");

  let mut cmd = Command::new(env!("CARGO_BIN_EXE_ui_perf_smoke"));
  cmd
    .current_dir(temp.path())
    // Ensure the harness never waits on network I/O under the default offline policy.
    .args(["--only", "network_denied", "--rayon-threads", "1", "--output"])
    .arg(output_path.to_str().expect("output path"))
    // Ensure we exercise the CLI override path (rather than inheriting a parent env var).
    .env_remove("RAYON_NUM_THREADS");

  let mut child = cmd.spawn().expect("spawn ui_perf_smoke");
  let start = Instant::now();
  let hard_limit = Duration::from_secs(30);
  let status = loop {
    if let Some(status) = child.try_wait().expect("try_wait ui_perf_smoke") {
      break status;
    }
    if start.elapsed() > hard_limit {
      let _ = child.kill();
      let _ = child.wait();
      panic!("ui_perf_smoke exceeded hard limit of {hard_limit:?}");
    }
    thread::sleep(Duration::from_millis(20));
  };

  assert!(
    output_path.is_file(),
    "expected ui_perf_smoke to write output json at {} (status={status:?})",
    output_path.display()
  );
  let raw = fs::read_to_string(&output_path).expect("read ui_perf_smoke json");
  let json: Value = serde_json::from_str(&raw).expect("parse ui_perf_smoke json");

  assert_eq!(
    json["run_config"]["allow_network"],
    Value::Bool(false),
    "expected allow_network=false, got {raw}"
  );
  assert_eq!(
    json["run_config"]["rayon_threads_source"],
    Value::String("cli".to_string()),
    "expected rayon_threads_source=cli when --rayon-threads is provided, got {raw}"
  );

  let scenarios = json["scenarios"]
    .as_array()
    .expect("scenarios array must exist");
  assert_eq!(scenarios.len(), 1, "expected --only to run one scenario");
  let scenario = &scenarios[0];
  assert_eq!(
    scenario["name"].as_str(),
    Some("network_denied"),
    "expected network_denied scenario, got {raw}"
  );
  assert_eq!(
    scenario["status"],
    Value::String("error".to_string()),
    "expected navigation to fail under offline policy, got {raw}"
  );
  let error = scenario["error"].as_str().unwrap_or_default();
  assert!(
    error.contains("fetch blocked by policy"),
    "expected policy denial error, got: {error} (full json: {raw})"
  );
}
