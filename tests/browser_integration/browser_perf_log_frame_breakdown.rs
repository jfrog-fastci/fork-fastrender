#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn perf_log_frame_event_includes_breakdown_fields() {
  let _lock = crate::browser_integration::stage_listener_test_lock();
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-smoke")
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    .env("FASTR_PERF_LOG", "1")
    .output()
    .expect("spawn browser --headless-smoke with perf log");

  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("HEADLESS_SMOKE_OK"),
    "expected headless smoke success marker, got stdout:\n{stdout}"
  );

  let mut found_frame = false;
  let mut found_memory = false;
  for line in stdout.lines() {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
      continue;
    };
    if value.get("event").and_then(|v| v.as_str()) != Some("frame") {
      if value.get("event").and_then(|v| v.as_str()) == Some("memory_summary") {
        assert!(
          value.get("rss_bytes").is_some(),
          "expected memory_summary to include rss_bytes field, got: {value}"
        );
        assert!(
          value.get("rss_mb").is_some(),
          "expected memory_summary to include rss_mb field, got: {value}"
        );
        assert!(
          value["rss_bytes"].is_null() || value["rss_bytes"].as_u64().is_some(),
          "expected rss_bytes to be null or integer, got: {value}"
        );
        assert!(
          value["rss_mb"].is_null() || value["rss_mb"].as_f64().is_some(),
          "expected rss_mb to be null or number, got: {value}"
        );
        found_memory = true;
      }
      continue;
    }

    for key in [
      "worker_msgs_ms",
      "upload_ms",
      "egui_ms",
      "tessellate_ms",
      "wgpu_ms",
      "present_ms",
      "total_ms",
    ] {
      assert!(
        value.get(key).and_then(|v| v.as_f64()).is_some_and(|v| v >= 0.0 && v.is_finite()),
        "expected {key} to be a non-negative finite number, got: {value}"
      );
    }

    found_frame = true;
    break;
  }

  assert!(
    found_frame,
    "expected at least one PERF frame event in stdout, got:\n{stdout}"
  );
  assert!(
    found_memory,
    "expected at least one PERF memory_summary event in stdout, got:\n{stdout}"
  );
}
