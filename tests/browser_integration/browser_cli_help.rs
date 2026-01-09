#![cfg(feature = "browser_ui")]

use std::process::Command;

#[test]
fn browser_help_exits_successfully_without_startup_logs() {
  let output = Command::new(env!("CARGO_BIN_EXE_browser"))
    .arg("--help")
    .output()
    .expect("spawn browser --help");

  assert!(
    output.status.success(),
    "browser --help exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("Usage:"),
    "expected help usage in stdout, got:\n{stdout}"
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB"),
    "expected --help to exit before startup/mem-limit logging, got stderr:\n{stderr}"
  );
}

