#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn perf_log_frame_event_includes_breakdown_fields() {
  let _lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");
  let bookmarks_path = dir.path().join("bookmarks.json");
  let history_path = dir.path().join("history.json");

  // Force a deterministic `about:` navigation so perf-log stage events are predictable.
  let session_json = r#"{
    "version": 2,
    "windows": [{
      "tabs": [{"url": "about:newtab"}],
      "active_tab_index": 0
    }],
    "active_window_index": 0
  }"#;

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-smoke")
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    .env("FASTR_BROWSER_SESSION_PATH", &session_path)
    .env("FASTR_BROWSER_BOOKMARKS_PATH", &bookmarks_path)
    .env("FASTR_BROWSER_HISTORY_PATH", &history_path)
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON", session_json)
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
  let mut found_stage = false;
  let mut first_frame_line: Option<usize> = None;
  let mut first_stage_line: Option<usize> = None;

  for (idx, line) in stdout.lines().enumerate() {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
      continue;
    };
    match value.get("event").and_then(|v| v.as_str()) {
      Some("memory_summary") => {
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
      Some("stage") => {
        if first_stage_line.is_none() {
          first_stage_line = Some(idx);
        }
        assert!(
          value.get("t_ms").and_then(|v| v.as_u64()).is_some(),
          "expected stage event to include integer t_ms field, got: {value}"
        );
        assert!(
          value.get("tab_id").and_then(|v| v.as_u64()).is_some(),
          "expected stage event to include integer tab_id field, got: {value}"
        );
        assert!(
          value
            .get("stage")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty()),
          "expected stage event to include non-empty stage field, got: {value}"
        );
        assert!(
          value
            .get("hotspot")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty()),
          "expected stage event to include non-empty hotspot field, got: {value}"
        );
        found_stage = true;
      }
      Some("frame") => {
        if first_frame_line.is_none() {
          first_frame_line = Some(idx);
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
      }
      _ => {}
    }
  }

  assert!(
    found_frame,
    "expected at least one PERF frame event in stdout, got:\n{stdout}"
  );
  assert!(
    found_memory,
    "expected at least one PERF memory_summary event in stdout, got:\n{stdout}"
  );
  assert!(
    found_stage,
    "expected at least one PERF stage event in stdout, got:\n{stdout}"
  );

  if let (Some(stage_line), Some(frame_line)) = (first_stage_line, first_frame_line) {
    assert!(
      stage_line < frame_line,
      "expected a stage event before the first frame event (stage@{stage_line}, frame@{frame_line})\nstdout:\n{stdout}"
    );
  }
}
