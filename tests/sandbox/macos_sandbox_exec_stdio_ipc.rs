//! macOS-only integration test asserting the `sandbox-exec` spawn path preserves stdio IPC.
//!
//! This is intentionally separate from the filesystem/network denial checks (see `macos_sandbox_exec.rs`).

#![cfg(target_os = "macos")]

use fastrender::sandbox::macos_spawn::wrap_command_with_sandbox_exec;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const ENV_STDIN_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDIN_SENTINEL";
const ENV_STDOUT_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDOUT_SENTINEL";

#[test]
fn sandbox_exec_spawn_preserves_stdio_pipes_for_ipc() {
  let child_exe = env!("CARGO_BIN_EXE_sandbox_exec_stdio_ipc_child");
  let sandbox_exec_path = Path::new("/usr/bin/sandbox-exec");
  if !sandbox_exec_path.is_file() {
    eprintln!(
      "skipping: sandbox-exec missing at {}",
      sandbox_exec_path.display()
    );
    return;
  }

  let cmd = Command::new(child_exe);
  let sbpl = "(version 1)\n(allow default)\n";
  let mut cmd = wrap_command_with_sandbox_exec(&cmd, sbpl)
    .expect("wrap command with sandbox-exec")
    .expect("expected sandbox-exec wrapper");

  let stdin_sentinel = "fastrender-stdio-ipc-in";
  let stdout_sentinel = "fastrender-stdio-ipc-out";

  cmd
    .env(ENV_STDIN_SENTINEL, stdin_sentinel)
    .env(ENV_STDOUT_SENTINEL, stdout_sentinel)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit());

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
