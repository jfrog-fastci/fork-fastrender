//! macOS sandbox-exec integration tests.
//!
//! These tests validate that the `/usr/bin/sandbox-exec` spawn wrapper preserves stdio-based IPC
//! (pipes). The actual filesystem/network sandbox denials are covered elsewhere; this test focuses
//! purely on pipe viability.

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::process::Stdio;

const ENV_STDIN_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDIN_SENTINEL";
const ENV_STDOUT_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDOUT_SENTINEL";

#[test]
fn sandbox_exec_spawn_preserves_stdio_pipes_for_ipc() {
  let child_exe = env!("CARGO_BIN_EXE_sandbox_exec_stdio_ipc_child");

  let stdin_sentinel = "fastrender-stdio-ipc-in";
  let stdout_sentinel = "fastrender-stdio-ipc-out";

  let mut cmd = crate::common::macos_sandbox_exec::sandbox_exec_command(
    child_exe,
    crate::common::macos_sandbox_exec::profile_allow_default(),
  );
  cmd.env(ENV_STDIN_SENTINEL, stdin_sentinel);
  cmd.env(ENV_STDOUT_SENTINEL, stdout_sentinel);
  cmd.stdin(Stdio::piped());
  cmd.stdout(Stdio::piped());
  cmd.stderr(Stdio::inherit());

  let mut child = cmd.spawn().expect("spawn sandbox-exec child");

  {
    let mut stdin = child.stdin.take().expect("child stdin");
    stdin
      .write_all(format!("{stdin_sentinel}\n").as_bytes())
      .expect("write stdin sentinel");
    stdin.flush().expect("flush stdin");
  }

  let mut output = String::new();
  child
    .stdout
    .take()
    .expect("child stdout")
    .read_to_string(&mut output)
    .expect("read child stdout");

  let status = child.wait().expect("wait for child");
  assert!(
    status.success(),
    "sandboxed child should exit 0 (status={status}, stdout={output:?})"
  );

  assert!(
    output.lines().any(|line| line.trim_end() == stdout_sentinel),
    "expected stdout to contain sentinel {stdout_sentinel:?}, got {output:?}"
  );
}

