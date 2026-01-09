#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::{Command, Output};

const MEM_LIMIT_KEY: &str = "FASTR_BROWSER_MEM_LIMIT_MB";

fn run_browser_with_mem_limit(value: &str) -> Output {
  Command::new(env!("CARGO_BIN_EXE_browser"))
    .env(MEM_LIMIT_KEY, value)
    // Keep this test headless: exit after parsing/applying the limit, before winit/wgpu init.
    .env("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY", "1")
    .output()
    .expect("spawn browser")
}

fn assert_success(output: &Output) {
  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn browser_applies_mem_limit_from_env_and_exits() {
  let output = run_browser_with_mem_limit("1024");
  assert_success(&output);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line, got stderr:\n{stderr}"
  );
}

#[test]
fn browser_mem_limit_empty_value_disables_limit_and_exits_zero() {
  let output = run_browser_with_mem_limit("");
  assert_success(&output);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled"),
    "expected disabled status line, got stderr:\n{stderr}"
  );
}

#[test]
fn browser_mem_limit_zero_disables_limit_and_exits_zero() {
  let output = run_browser_with_mem_limit("0");
  assert_success(&output);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled"),
    "expected disabled status line, got stderr:\n{stderr}"
  );
}

#[test]
fn browser_mem_limit_invalid_value_disables_limit_and_exits_zero() {
  let output = run_browser_with_mem_limit("not-a-number");
  assert_success(&output);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Disabled (invalid value:"),
    "expected invalid-value status line, got stderr:\n{stderr}"
  );
  assert!(
    stderr.contains("not-a-number"),
    "expected stderr to mention the invalid value, got stderr:\n{stderr}"
  );
}

#[test]
fn browser_mem_limit_accepts_underscores() {
  let output = run_browser_with_mem_limit("1_024");
  assert_success(&output);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line for underscore value, got stderr:\n{stderr}"
  );
}
