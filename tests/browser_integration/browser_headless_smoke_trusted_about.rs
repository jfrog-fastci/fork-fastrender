#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn browser_headless_smoke_renders_about_newtab_without_renderer_worker() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Force the session to start on `about:newtab` so the trusted about renderer path is exercised.
  let session_json = r#"{
    "version": 2,
    "windows": [{
      "tabs": [{"url": "about:newtab"}],
      "active_tab_index": 0
    }],
    "active_window_index": 0
  }"#;

  let run_limited = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-smoke")
    .env("RAYON_NUM_THREADS", "1")
    .env("FASTR_BROWSER_SESSION_PATH", &session_path)
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON", session_json)
    // Simulate the renderer worker being unavailable (spawn should fail if attempted).
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE_DISABLE_WORKER", "1")
    .output()
    .expect("spawn browser");

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
}

