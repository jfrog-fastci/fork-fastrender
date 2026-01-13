//! macOS Seatbelt sandbox wrapper.
//!
//! This module is intentionally `cfg(target_os = "macos")`-only. Non-macOS platforms should use
//! their native sandboxing mechanisms.
//!
//! ## Why `-f` instead of `-p`
//!
//! `sandbox-exec` accepts an inline profile string via `-p`, but real profiles tend to be
//! multi-line and can exceed argument length limits. Passing them inline also makes quoting and
//! debugging painful. We instead write custom profiles to a private temporary file and pass that
//! path to `sandbox-exec -f`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output};

/// Standard Seatbelt parameters passed to sandbox profiles.
///
/// `sandbox-exec` exposes these through `-D KEY=value` and SBPL can reference them via `(param
/// "KEY")`. Keeping them out of the profile string avoids string-interpolation and escaping bugs
/// (especially when values contain spaces or quotes).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeatbeltParameters {
  home: String,
  tmpdir: String,
}

impl SeatbeltParameters {
  fn from_env() -> Self {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
    let tmpdir =
      std::env::var("TMPDIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    Self { home, tmpdir }
  }

  fn sandbox_exec_definitions(&self) -> Vec<String> {
    vec![
      "-D".to_string(),
      format!("HOME={}", self.home),
      "-D".to_string(),
      format!("TMPDIR={}", self.tmpdir),
    ]
  }
}

fn push_sandbox_exec_parameters(cmd: &mut Command, params: &SeatbeltParameters) {
  // `sandbox-exec` uses getopt: each `-D` consumes the following argv entry.
  for def in params.sandbox_exec_definitions() {
    cmd.arg(def);
  }
}

/// Seatbelt profile selection for `sandbox-exec`.
#[derive(Debug, Clone)]
pub enum SandboxExecProfile {
  /// Use a named profile (no temp file needed).
  ///
  /// Example: `"pure-computation"`.
  Named(OsString),
  /// Use a custom, multi-line profile string.
  ///
  /// The wrapper writes this string to a private temporary file (mode `0600`) and invokes
  /// `sandbox-exec -f <profile_file>`.
  Custom(String),
}

impl SandboxExecProfile {
  /// Convenience constructor for the built-in `pure-computation` profile.
  pub fn pure_computation() -> Self {
    Self::Named(OsString::from("pure-computation"))
  }
}

/// A `sandbox-exec` command wrapper that cleans up any temporary profile file on spawn.
///
/// When `profile` is [`SandboxExecProfile::Custom`], the profile is written to a secure temporary
/// file (mode `0600`). The file is removed immediately after spawning the sandboxed child process
/// (best-effort).
///
/// If spawning fails, the temporary file is still dropped and will be removed best-effort.
#[derive(Debug)]
pub struct SandboxExecCommand {
  cmd: Command,
  profile_file: Option<tempfile::NamedTempFile>,
  #[allow(dead_code)]
  profile_path: Option<PathBuf>,
}

impl SandboxExecCommand {
  const SANDBOX_EXEC: &'static str = "/usr/bin/sandbox-exec";

  /// Build a new command that runs `program` (with `args`) under `sandbox-exec`.
  pub fn new(
    profile: SandboxExecProfile,
    program: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
  ) -> io::Result<Self> {
    let sandbox_exec = Path::new(Self::SANDBOX_EXEC);
    if !sandbox_exec.exists() {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing {}", sandbox_exec.display()),
      ));
    }

    let mut cmd = Command::new(sandbox_exec);
    let mut profile_file: Option<tempfile::NamedTempFile> = None;
    let mut profile_path: Option<PathBuf> = None;

    match profile {
      SandboxExecProfile::Named(name) => {
        cmd.arg("-n").arg(name);
      }
      SandboxExecProfile::Custom(contents) => {
        use std::os::unix::fs::PermissionsExt;

        // Make common Seatbelt parameters available to the profile via `(param "HOME")`, etc.
        // Do this before `-f` so the resulting argv is:
        // `sandbox-exec -D HOME=... -D TMPDIR=... -f <profile> -- <program> ...`.
        push_sandbox_exec_parameters(&mut cmd, &SeatbeltParameters::from_env());

        let mut tmp = tempfile::Builder::new()
          .prefix("fastr-sandbox-profile-")
          .suffix(".sb")
          .tempfile()?;
        // Ensure the profile file is private to the current user (defense in depth; `tempfile`
        // already creates files as 0600).
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;

        tmp.as_file_mut().write_all(contents.as_bytes())?;
        tmp.as_file_mut().flush()?;

        let path = tmp.path().to_path_buf();
        cmd.arg("-f").arg(&path);
        profile_path = Some(path);
        profile_file = Some(tmp);
      }
    }

    cmd.arg("--").arg(program);
    for arg in args {
      cmd.arg(arg);
    }

    Ok(Self {
      cmd,
      profile_file,
      profile_path,
    })
  }

  /// Mutable access to the underlying `Command` (e.g. to set env vars or stdio).
  pub fn command_mut(&mut self) -> &mut Command {
    &mut self.cmd
  }

  /// Spawn the sandboxed command.
  ///
  /// The temporary profile file (if any) is removed immediately after spawning (best-effort).
  pub fn spawn(mut self) -> io::Result<Child> {
    let result = self.cmd.spawn();
    // Drop the temp file after spawning so the profile does not linger on disk while the sandboxed
    // process runs. This is best-effort: if removal fails, we intentionally ignore the error.
    drop(self.profile_file.take());
    drop(self.profile_path.take());
    result
  }

  /// Run the command to completion and return its exit status.
  pub fn status(self) -> io::Result<ExitStatus> {
    let mut child = self.spawn()?;
    child.wait()
  }

  /// Run the command to completion and capture its output.
  pub fn output(self) -> io::Result<Output> {
    let mut child = self.spawn()?;
    child.wait_with_output()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sandbox_exec_definitions_preserve_spaces() {
    let params = SeatbeltParameters {
      home: "/Users/Test User".to_string(),
      tmpdir: "/var/folders/xx/Some Tmp".to_string(),
    };

    assert_eq!(
      params.sandbox_exec_definitions(),
      vec![
        "-D",
        "HOME=/Users/Test User",
        "-D",
        "TMPDIR=/var/folders/xx/Some Tmp"
      ]
    );
  }

  #[test]
  fn sandbox_exec_arg_construction_keeps_spacey_values_as_single_argv_entries() {
    let params = SeatbeltParameters {
      home: "/Users/Test User".to_string(),
      tmpdir: "/var/folders/xx/Some Tmp".to_string(),
    };

    let mut cmd = Command::new("sandbox-exec");
    push_sandbox_exec_parameters(&mut cmd, &params);
    cmd
      .arg("-f")
      .arg("/tmp/profile.sb")
      .arg("--")
      .arg("echo")
      .arg("hello");

    let args: Vec<String> = cmd
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect();
    assert_eq!(
      args,
      vec![
        "-D",
        "HOME=/Users/Test User",
        "-D",
        "TMPDIR=/var/folders/xx/Some Tmp",
        "-f",
        "/tmp/profile.sb",
        "--",
        "echo",
        "hello"
      ]
    );
  }
}
