//! macOS sandbox helpers for integration tests.
//!
//! We use `/usr/bin/sandbox-exec` (pre-main sandboxing) to validate that the spawn wrapper used for
//! renderer processes does not break stdio-based IPC (pipes).

#![cfg(target_os = "macos")]

use std::process::Command;

const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Minimal sandbox profile that keeps behaviour as close as possible to an unsandboxed process.
///
/// This test helper is intentionally permissive because the IPC-focused tests want to ensure that
/// `sandbox-exec` invocation itself preserves inherited stdio pipes; filesystem/network denials are
/// exercised separately.
pub(crate) fn profile_allow_default() -> &'static str {
  "(version 1)\n(allow default)\n"
}

/// Create a [`Command`] that runs the given executable under `/usr/bin/sandbox-exec`.
///
/// The caller is expected to configure stdio (`stdin/stdout/stderr`) on the returned command.
pub(crate) fn sandbox_exec_command(exe: &str, profile: &str) -> Command {
  let mut cmd = Command::new(SANDBOX_EXEC_PATH);
  cmd.arg("-p").arg(profile).arg(exe);
  cmd
}

