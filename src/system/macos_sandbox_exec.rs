//! macOS sandbox-exec helpers.
//!
//! This module provides a pragmatic fallback for applying a Seatbelt sandbox profile when
//! in-process sandboxing (`sandbox_init`) is unavailable or when developers want to experiment
//! with SBPL profiles externally.
//!
//! IMPORTANT: `sandbox-exec` is deprecated by Apple and may be removed in future macOS releases.
//! Treat this as a debug/legacy mechanism, not a long-term sandboxing strategy.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;

/// Opt-in env var gate for wrapping child processes with `sandbox-exec`.
pub const ENV_MACOS_USE_SANDBOX_EXEC: &str = "FASTR_MACOS_USE_SANDBOX_EXEC";

fn parse_env_bool(raw: Option<&str>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return false;
  }
  match raw.to_ascii_lowercase().as_str() {
    "0" | "false" | "no" | "off" => false,
    _ => true,
  }
}

/// Returns `true` when `FASTR_MACOS_USE_SANDBOX_EXEC` is set to an enabled value.
pub fn macos_use_sandbox_exec_from_env() -> bool {
  parse_env_bool(std::env::var(ENV_MACOS_USE_SANDBOX_EXEC).ok().as_deref())
}

/// Conditionally wrap `cmd` under `sandbox-exec` when `FASTR_MACOS_USE_SANDBOX_EXEC` is enabled.
pub fn maybe_wrap_command_with_sandbox_exec(cmd: &mut Command, sbpl: &str) -> io::Result<()> {
  if macos_use_sandbox_exec_from_env() {
    wrap_command_with_sandbox_exec(cmd, sbpl)?;
  }
  Ok(())
}

/// Rewrite `cmd` so it executes under `sandbox-exec`.
///
/// Specifically, the command is transformed to:
/// `sandbox-exec -p <sbpl> -- <original-exe> <args...>`
///
/// This helper does **not** invoke a shell; all arguments are forwarded as separate argv entries.
///
/// Note: The rewrite constructs a new [`Command`] internally. Environment overrides and
/// `current_dir` are preserved, but other configuration (notably stdio) should be applied after
/// calling this helper.
pub fn wrap_command_with_sandbox_exec(cmd: &mut Command, sbpl: &str) -> io::Result<()> {
  if sbpl.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "SBPL profile string is empty",
    ));
  }
  if sbpl.as_bytes().contains(&0) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "SBPL profile string contains NUL byte",
    ));
  }

  let original_program = cmd.get_program().to_os_string();
  if original_program.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "command program is empty",
    ));
  }
  let current_dir: Option<PathBuf> = cmd.get_current_dir().map(PathBuf::from);

  let mut wrapped = Command::new("sandbox-exec");
  wrapped
    .arg("-p")
    .arg(sbpl)
    .arg("--")
    .arg(&original_program)
    .args(cmd.get_args());

  if let Some(dir) = current_dir {
    wrapped.current_dir(dir);
  }
  for (key, value) in cmd.get_envs() {
    match value {
      Some(value) => {
        wrapped.env(key, value);
      }
      None => {
        wrapped.env_remove(key);
      }
    }
  }

  *cmd = wrapped;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::OsStr;
  use std::net::TcpListener;
  use std::sync::{Mutex, OnceLock};

  static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

  fn env_lock() -> &'static Mutex<()> {
    ENV_LOCK.get_or_init(|| Mutex::new(()))
  }

  #[test]
  fn sandbox_exec_blocks_network_bind() {
    const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_EXEC_CHILD";
    const EXPECT_ENV: &str = "FASTR_TEST_SANDBOX_EXEC_EXPECT_BIND_OK";

    if std::env::var_os(CHILD_ENV).is_some() {
      let expect_ok = std::env::var_os(EXPECT_ENV)
        .as_deref()
        .is_some_and(|v| v == OsStr::new("1"));
      let result = TcpListener::bind(("127.0.0.1", 0));
      if expect_ok {
        assert!(
          result.is_ok(),
          "expected network bind to succeed, got: {:?}",
          result.err()
        );
      } else {
        assert!(
          result.is_err(),
          "expected network bind to be blocked by sandbox-exec"
        );
      }
      return;
    }

    let _guard = env_lock().lock().unwrap();

    // Skip the test if sandbox-exec is missing (common on some minimal macOS images).
    match Command::new("sandbox-exec").arg("-h").output() {
      Ok(_) => {}
      Err(err) if err.kind() == io::ErrorKind::NotFound => {
        eprintln!("skipping: sandbox-exec not found in PATH");
        return;
      }
      Err(err) => panic!("failed to probe sandbox-exec: {err}"),
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "system::macos_sandbox_exec::tests::sandbox_exec_blocks_network_bind";

    // First validate that network bind works *without* sandbox-exec so we know the environment is
    // capable of binding localhost.
    let baseline = Command::new(&exe)
      .env(CHILD_ENV, "1")
      .env(EXPECT_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn baseline child process");
    if !baseline.status.success() {
      eprintln!(
        "skipping: baseline child could not bind localhost (stdout={}, stderr={})",
        String::from_utf8_lossy(&baseline.stdout),
        String::from_utf8_lossy(&baseline.stderr)
      );
      return;
    }

    let sbpl = "(version 1)\n(allow default)\n(deny network*)\n";
    let mut cmd = Command::new(&exe);
    cmd
      .env(CHILD_ENV, "1")
      .env(EXPECT_ENV, "0")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture");
    wrap_command_with_sandbox_exec(&mut cmd, sbpl).expect("wrap command with sandbox-exec");
    let output = cmd.output().expect("spawn sandboxed child process");
    assert!(
      output.status.success(),
      "sandboxed child should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
