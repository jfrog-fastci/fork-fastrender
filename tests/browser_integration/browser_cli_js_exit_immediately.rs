#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_cli_js_exit_immediately() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .args(["--js", "--exit-immediately"])
    .output()
    .expect("spawn browser --js --exit-immediately");

  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);

  assert!(
    output.status.success(),
    "browser --js --exit-immediately exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    stderr,
    stdout
  );

  let combined = format!("{stderr}{stdout}");
  assert!(
    !combined.contains("warning: --js is currently supported only with --headless-smoke"),
    "expected browser --js --exit-immediately to avoid legacy warning, got:\n{combined}"
  );
}

