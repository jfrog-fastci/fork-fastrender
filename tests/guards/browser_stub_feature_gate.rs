#![cfg(not(feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_binary_without_browser_ui_feature_is_stub() {
  let output = Command::new(env!("CARGO_BIN_EXE_browser"))
    .output()
    .expect("run browser stub");

  assert_eq!(
    output.status.code(),
    Some(2),
    "expected browser stub to exit with code 2; status={:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("requires the `browser_ui` feature"),
    "expected stub error message on stderr, got:\n{stderr}"
  );
  assert!(
    stderr.contains("bash scripts/run_limited.sh"),
    "expected stub to print the wrapper-script run command, got:\n{stderr}"
  );
}
