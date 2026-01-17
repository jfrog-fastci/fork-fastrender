//! macOS Seatbelt sandbox wrapper.
//!
//! This module is intentionally `cfg(target_os = "macos")`-only. Non-macOS platforms should use
//! their native sandboxing mechanisms.
//!
//! ⚠️ Apple has deprecated `/usr/bin/sandbox-exec` and may remove it in future macOS releases.
//! FastRender keeps this wrapper primarily for debugging / legacy workflows. Prefer in-process
//! Seatbelt sandboxing via `sandbox_init(3)` (`src/sandbox/macos.rs`) for long-term sandboxing, and
//! treat any `sandbox-exec` usage as best-effort.
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
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output};
use std::process::Stdio;

/// Standard Seatbelt parameters passed to sandbox profiles.
///
/// `sandbox-exec` exposes these through `-D KEY=value` and SBPL can reference them via `(param
/// "KEY")`. Keeping them out of the profile string avoids string-interpolation and escaping bugs
/// (especially when values contain spaces or quotes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatbeltParameters {
  home: String,
  tmpdir: String,
}

impl SeatbeltParameters {
  pub(crate) fn new(home: String, tmpdir: String) -> Self {
    Self { home, tmpdir }
  }

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

/// RAII wrapper that keeps any temporary `sandbox-exec` profile file alive until spawn.
///
/// This is required when using `sandbox-exec -f <profile-file>` with a temporary file: if the
/// temp file is dropped before `Command::spawn()` is called, `sandbox-exec` will fail to open it.
#[derive(Debug)]
pub struct SandboxedCommand {
  cmd: Command,
  _profile_file: Option<tempfile::NamedTempFile>,
  used_temp_profile: bool,
}

impl SandboxedCommand {
  const SANDBOX_EXEC: &'static str = "/usr/bin/sandbox-exec";

  /// Build a new command that runs `program` (with `args`) under `sandbox-exec`.
  pub fn new(
    profile: SandboxExecProfile,
    program: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
  ) -> io::Result<Self> {
    Self::new_with_parameters(profile, SeatbeltParameters::from_env(), program, args)
  }

  /// Build a new command that runs `program` (with `args`) under `sandbox-exec`, using explicitly
  /// provided Seatbelt parameters.
  ///
  /// This is primarily used by higher-level wrappers that first construct a `Command` (with env
  /// overrides) and then wrap it. The Seatbelt params should match the environment that the
  /// sandboxed child will observe.
  pub(crate) fn new_with_parameters(
    profile: SandboxExecProfile,
    params: SeatbeltParameters,
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
    let used_temp_profile = matches!(profile, SandboxExecProfile::Custom(_));

    match profile {
      SandboxExecProfile::Named(name) => {
        cmd.arg("-n").arg(name);
      }
      SandboxExecProfile::Custom(contents) => {
        use std::os::unix::fs::PermissionsExt;

        // Make common Seatbelt parameters available to the profile via `(param "HOME")`, etc.
        // Do this before `-f` so the resulting argv is:
        // `sandbox-exec -D HOME=... -D TMPDIR=... -f <profile> -- <program> ...`.
        push_sandbox_exec_parameters(&mut cmd, &params);

        let mut tmp = tempfile::Builder::new()
          .prefix("fastr-sandbox-profile-")
          .suffix(".sb")
          .tempfile()?;
        // Ensure the profile file is private to the current user (defense in depth; `tempfile`
        // already creates files as 0600).
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;

        tmp.as_file_mut().write_all(contents.as_bytes())?;
        tmp.as_file_mut().flush()?;

        cmd.arg("-f").arg(tmp.path());
        profile_file = Some(tmp);
      }
    }

    cmd.arg("--").arg(program);
    for arg in args {
      cmd.arg(arg);
    }

    Ok(Self {
      cmd,
      _profile_file: profile_file,
      used_temp_profile,
    })
  }

  /// Mutable access to the underlying `Command` (e.g. to set env vars or stdio).
  pub fn command_mut(&mut self) -> &mut Command {
    &mut self.cmd
  }

  /// Spawn the sandboxed command.
  ///
  /// When a temporary profile file is used, it is dropped (and therefore deleted) immediately
  /// after the child process has been created.
  ///
  /// If spawning fails, the temp file is kept alive until this command is dropped, at which point
  /// it is removed best-effort.
  pub fn spawn(&mut self) -> io::Result<Child> {
    if self.used_temp_profile && self._profile_file.is_none() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "sandbox-exec command already spawned (temporary profile file consumed)",
      ));
    }
    let child = self.cmd.spawn()?;
    // Drop the temp file only after spawn succeeds.
    self._profile_file = None;
    Ok(child)
  }

  /// Run the command to completion and return its exit status.
  pub fn status(&mut self) -> io::Result<ExitStatus> {
    let mut child = self.spawn()?;
    child.wait()
  }

  /// Run the command to completion and capture its output.
  pub fn output(&mut self) -> io::Result<Output> {
    // Mirror `Command::output()` semantics: capture stdout/stderr regardless of prior stdio config.
    //
    // Without this, `wait_with_output()` errors if the caller did not explicitly request piped
    // output.
    self
      .cmd
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    let child = self.spawn()?;
    child.wait_with_output()
  }
}

impl Deref for SandboxedCommand {
  type Target = Command;

  fn deref(&self) -> &Self::Target {
    &self.cmd
  }
}

impl DerefMut for SandboxedCommand {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.cmd
  }
}

/// Backwards compatible alias for the older wrapper name.
pub type SandboxExecCommand = SandboxedCommand;

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
