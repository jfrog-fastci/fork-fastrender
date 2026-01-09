#![cfg(feature = "browser_ui")]

use std::process::Command;

#[test]
fn browser_headless_smoke_mode_runs_and_reports_success() {
  let output = Command::new(env!("CARGO_BIN_EXE_browser"))
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
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

