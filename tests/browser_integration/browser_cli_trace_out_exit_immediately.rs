#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_cli_trace_out_exit_immediately_writes_trace_file() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let trace_path = dir.path().join("browser_trace.json");

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--trace-out")
    .arg(&trace_path)
    .arg("--exit-immediately")
    // Avoid inherited env vars overriding the requested path or changing trace retention behavior.
    .env_remove("FASTR_BROWSER_TRACE_OUT")
    .env_remove("FASTR_PERF_TRACE_OUT")
    .env_remove("FASTR_TRACE_MAX_EVENTS")
    .output()
    .expect("spawn browser --trace-out --exit-immediately");

  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);

  assert!(
    output.status.success(),
    "browser --trace-out --exit-immediately exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    stderr,
    stdout
  );

  let raw = std::fs::read_to_string(&trace_path)
    .unwrap_or_else(|err| panic!("expected trace file at {}: {err}", trace_path.display()));
  let parsed: serde_json::Value =
    serde_json::from_str(&raw).expect("trace JSON should be parseable");
  assert!(
    parsed.get("traceEvents").is_some(),
    "expected traceEvents key, got: {parsed}"
  );
}

