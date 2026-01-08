#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_applies_mem_limit_from_env_and_exits() {
  let output = Command::new(env!("CARGO_BIN_EXE_browser"))
    .env("FASTR_BROWSER_MEM_LIMIT_MB", "1024")
    // Keep this test headless: exit after parsing/applying the limit, before winit/wgpu init.
    .env("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY", "1")
    .output()
    .expect("spawn browser");

  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("FASTR_BROWSER_MEM_LIMIT_MB: Applied (1024 MiB)"),
    "expected mem-limit status line, got stderr:\n{stderr}"
  );
}

