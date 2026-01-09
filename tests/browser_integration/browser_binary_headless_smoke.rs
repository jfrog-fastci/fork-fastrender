#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_binary_headless_smoke_mode_runs_without_window() {
  let _lock = super::stage_listener_test_lock();
  let output = Command::new(env!("CARGO_BIN_EXE_browser"))
    .env("FASTR_BROWSER_MEM_LIMIT_MB", "1024")
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE", "1")
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
