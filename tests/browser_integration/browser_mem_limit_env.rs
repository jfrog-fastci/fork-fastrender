#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::{Command, ExitStatus};

fn run_browser_with_mem_env(value: Option<&str>) -> (ExitStatus, String, String) {
  let mut cmd = Command::new(env!("CARGO_BIN_EXE_browser"));
  match value {
    Some(value) => {
      cmd.env("FASTR_BROWSER_MEM_LIMIT_MB", value);
    }
    None => {
      cmd.env_remove("FASTR_BROWSER_MEM_LIMIT_MB");
    }
  }
  // Keep this test headless: exit after parsing/applying the limit, before winit/wgpu init.
  cmd.env("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY", "1");
  let output = cmd.output().expect("spawn browser");

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  (output.status, stderr, stdout)
}

fn assert_browser_succeeded(status: ExitStatus, stderr: &str, stdout: &str) {
  assert!(
    status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    status.code(),
    stderr,
    stdout
  );
}

#[test]
fn browser_reports_mem_limit_disabled_when_env_unset_and_exits() {
  let (status, stderr, stdout) = run_browser_with_mem_env(None);
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled"),
    "expected mem-limit status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_applies_mem_limit_from_env_and_exits() {
  let (status, stderr, stdout) = run_browser_with_mem_env(Some("1024"));
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_applies_mem_limit_with_underscore_separators() {
  let (status, stderr, stdout) = run_browser_with_mem_env(Some("1_024"));
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_applies_mem_limit_with_ascii_whitespace() {
  let (status, stderr, stdout) = run_browser_with_mem_env(Some(" 1024 "));
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_disables_mem_limit_for_empty_or_whitespace_only_value() {
  for value in ["", "   "] {
    let (status, stderr, stdout) = run_browser_with_mem_env(Some(value));
    assert_browser_succeeded(status, &stderr, &stdout);

    assert!(
      stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled"),
      "expected mem-limit to be disabled for value {value:?}, got stderr:\n{stderr}\nstdout:\n{stdout}"
    );
  }
}

#[test]
fn browser_disables_mem_limit_for_zero() {
  let (status, stderr, stdout) = run_browser_with_mem_env(Some("0"));
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled"),
    "expected mem-limit status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_disables_mem_limit_for_invalid_non_numeric_value() {
  let value = "not-a-number";
  let (status, stderr, stdout) = run_browser_with_mem_env(Some(value));
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled (invalid value:"),
    "expected invalid-value status line, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains(value),
    "expected stderr to mention invalid value {value:?}, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
}
