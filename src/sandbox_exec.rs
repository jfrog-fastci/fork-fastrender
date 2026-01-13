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

