#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_help_exits_successfully_without_startup_logs() {
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
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
  assert!(
    stdout.contains("Supported schemes:"),
    "expected help to mention supported schemes, got:\n{stdout}"
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB"),
    "expected --help to exit before startup/mem-limit logging, got stderr:\n{stderr}"
  );
}
