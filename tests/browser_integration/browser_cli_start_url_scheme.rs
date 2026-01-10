#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

fn run_browser_with_url(url: &str) -> (std::process::ExitStatus, String, String) {
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--exit-immediately")
    .arg(url)
    .output()
    .expect("spawn browser");

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  (output.status, stderr, stdout)
}

fn assert_browser_succeeded(status: std::process::ExitStatus, stderr: &str, stdout: &str) {
  assert!(
    status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    status.code(),
    stderr,
    stdout
  );
}

#[test]
fn browser_rejects_javascript_start_url_and_falls_back_to_newtab() {
  let _lock = super::stage_listener_test_lock();
  let (status, stderr, stdout) = run_browser_with_url("javascript:alert(1)");
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("invalid start URL"),
    "expected browser to report invalid start URL, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains("javascript"),
    "expected stderr to mention javascript scheme, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains("falling back to about:newtab"),
    "expected fallback to about:newtab, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_rejects_unknown_scheme_start_url_and_falls_back_to_newtab() {
  let _lock = super::stage_listener_test_lock();
  let (status, stderr, stdout) = run_browser_with_url("foo:bar");
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("invalid start URL"),
    "expected browser to report invalid start URL, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains("unsupported URL scheme: foo"),
    "expected unsupported-scheme message, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains("falling back to about:newtab"),
    "expected fallback to about:newtab, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}
