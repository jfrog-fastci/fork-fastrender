//! macOS renderer-launch sandboxing helpers.
//!
//! # `sandbox-exec` vs `sandbox_init`
//!
//! macOS exposes the "Seatbelt" sandbox through two main interfaces:
//!
//! - **`/usr/bin/sandbox-exec` (recommended for pre-main sandboxing):**
//!   The browser process launches the renderer *through* `sandbox-exec`, so the renderer starts
//!   executing already sandboxed. This is useful when the parent (browser) process is
//!   multithreaded and cannot safely use `std::os::unix::process::CommandExt::pre_exec`, which runs
//!   after `fork()` and is undefined/unsafe in a multithreaded parent.
//!
//! - **`sandbox_init(3)` (recommended for in-process sandboxing):**
//!   The renderer calls the Seatbelt API itself very early in `main` (or earlier) to apply a
//!   sandbox profile. This avoids the external `sandbox-exec` dependency and can be easier to
//!   parameterize (e.g. per-site policies), but it leaves a larger window where process startup
//!   runs unsandboxed, and it requires careful auditing to ensure the sandbox is installed before
//!   any untrusted work (or helper threads) begin.
//!
//! In a multiprocess browser architecture, `sandbox-exec` is a pragmatic way to ensure the
//! renderer's *entire* lifetime (including early initialization code) runs inside the sandbox.

#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
#[cfg(target_os = "macos")]
use std::{fs, io};

#[cfg(target_os = "macos")]
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "macos")]
const PURE_COMPUTATION_PROFILE_NAME: &str = "pure-computation";

/// Fallback sandbox profile used with `sandbox-exec -p ...` when `sandbox-exec -n` is unavailable.
///
/// The goal is to be "equivalent enough" to the built-in `pure-computation` profile for our use
/// case: deny filesystem access (outside of system dynamic-loader locations) and deny all network.
///
/// Notes:
/// - We intentionally do **not** whitelist `/private/etc` (where `/etc` resolves), so attempts to
///   read `/etc/passwd` should be denied.
/// - This profile still allows process execution (`process*`) so the target renderer can start.
#[cfg(target_os = "macos")]
const PURE_COMPUTATION_PROFILE_FALLBACK: &str = r#"(version 1)
(deny default)
(allow process*)
(allow file-read*
  (subpath "/System")
  (subpath "/usr/lib")
  (subpath "/usr/share")
  (subpath "/Library")
  (subpath "/dev")
  (subpath "/private/var/db")
)
"#;

#[cfg(target_os = "macos")]
const PURE_COMPUTATION_PROFILE_FILES: &[&str] = &[
  // Historical location for `sandbox-exec` sample profiles.
  "/usr/share/sandbox/pure-computation.sb",
  // Seatbelt profiles shipped with macOS.
  "/System/Library/Sandbox/Profiles/pure-computation.sb",
];

#[cfg(target_os = "macos")]
#[derive(Debug, thiserror::Error)]
pub enum SandboxExecError {
  #[error("`sandbox-exec` is required for pre-main sandboxing but was not found at {path}")]
  MissingSandboxExec { path: PathBuf },
  #[error("failed to probe sandbox-exec named profile support (-n)")]
  ProbeFailed {
    #[source]
    source: io::Error,
  },
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
enum SandboxExecInvocation {
  NamedProfile,
  InlineProfile { profile: String },
}

#[cfg(target_os = "macos")]
static INVOCATION: OnceLock<SandboxExecInvocation> = OnceLock::new();

#[cfg(target_os = "macos")]
fn sandbox_exec_invocation() -> Result<&'static SandboxExecInvocation, SandboxExecError> {
  if let Some(invocation) = INVOCATION.get() {
    return Ok(invocation);
  }

  let invocation = probe_sandbox_exec_invocation()?;
  // If multiple threads race to initialize, whichever wins sets the value; all others can use the
  // already-set value.
  let _ = INVOCATION.set(invocation);
  Ok(
    INVOCATION
      .get()
      .expect("OnceLock should contain invocation after initialization"),
  )
}

#[cfg(target_os = "macos")]
fn probe_sandbox_exec_invocation() -> Result<SandboxExecInvocation, SandboxExecError> {
  let sandbox_exec = Path::new(SANDBOX_EXEC_PATH);
  if !sandbox_exec.is_file() {
    return Err(SandboxExecError::MissingSandboxExec {
      path: sandbox_exec.to_path_buf(),
    });
  }

  // Probe whether `-n pure-computation` is supported.
  // `sandbox-exec` returns non-zero when `-n` is unsupported, or when the profile name is unknown.
  // In either case, fall back to `-p` with an inline profile.
  let probe = Command::new(SANDBOX_EXEC_PATH)
    .arg("-n")
    .arg(PURE_COMPUTATION_PROFILE_NAME)
    .arg("/usr/bin/true")
    .output();
  match probe {
    Ok(output) if output.status.success() => return Ok(SandboxExecInvocation::NamedProfile),
    Ok(_output) => {
      // Fall back to the inline profile.
    }
    Err(err) if err.kind() == io::ErrorKind::NotFound => {
      return Err(SandboxExecError::MissingSandboxExec {
        path: PathBuf::from(SANDBOX_EXEC_PATH),
      });
    }
    Err(err) => return Err(SandboxExecError::ProbeFailed { source: err }),
  };

  // Best-effort: if the system ships a `pure-computation` profile file, use its contents.
  // This keeps behaviour closer to the named profile on older OS versions.
  for path in PURE_COMPUTATION_PROFILE_FILES {
    let path = Path::new(path);
    if !path.is_file() {
      continue;
    }
    match fs::read_to_string(path) {
      Ok(profile) => return Ok(SandboxExecInvocation::InlineProfile { profile }),
      Err(_) => continue,
    }
  }

  Ok(SandboxExecInvocation::InlineProfile {
    profile: PURE_COMPUTATION_PROFILE_FALLBACK.to_string(),
  })
}

/// Build a [`Command`] that launches `renderer_path` through `/usr/bin/sandbox-exec`.
///
/// This is intended for the "browser" (trusted) process to spawn the "renderer" (untrusted)
/// process already sandboxed **without** relying on `CommandExt::pre_exec`.
///
/// The returned `Command` can be further configured by the caller (environment variables, stdio,
/// working directory, etc). Those settings apply to `sandbox-exec` and are inherited by the
/// renderer it `exec`s.
#[cfg(target_os = "macos")]
pub fn sandbox_exec_command(
  renderer_path: &Path,
  args: &[OsString],
) -> Result<Command, SandboxExecError> {
  let invocation = sandbox_exec_invocation()?;

  let mut cmd = Command::new(SANDBOX_EXEC_PATH);
  match invocation {
    SandboxExecInvocation::NamedProfile => {
      cmd.arg("-n").arg(PURE_COMPUTATION_PROFILE_NAME);
    }
    SandboxExecInvocation::InlineProfile { profile } => {
      cmd.arg("-p").arg(profile);
    }
  }

  cmd.arg(renderer_path);
  cmd.args(args);
  Ok(cmd)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::*;
  use std::io;
  use std::net::{TcpListener, TcpStream};

  // This test is macOS-only and ignored by default because it relies on the host having a working
  // Seatbelt sandbox (`sandbox-exec`). It's still valuable to run locally on macOS to validate that
  // the fallback profile blocks filesystem/network access.
  #[test]
  #[ignore]
  fn sandbox_exec_blocks_file_and_network() {
    const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_EXEC_CHILD";
    const PORT_ENV: &str = "FASTR_TEST_SANDBOX_EXEC_PORT";

    fn is_permission_error(err: &io::Error) -> bool {
      if err.kind() == io::ErrorKind::PermissionDenied {
        return true;
      }
      matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES))
    }

    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      let read_err = std::fs::read("/private/etc/passwd")
        .or_else(|err| {
          if err.kind() == io::ErrorKind::NotFound {
            std::fs::read("/etc/passwd")
          } else {
            Err(err)
          }
        })
        .expect_err("expected sandbox to block reading /etc/passwd");
      assert!(
        is_permission_error(&read_err),
        "expected file read to be denied by sandbox, got {read_err:?}"
      );

      let connect_err =
        TcpStream::connect(("127.0.0.1", port)).expect_err("expected sandbox to block TCP connect");
      assert!(
        is_permission_error(&connect_err),
        "expected connect to be denied by sandbox, got {connect_err:?}"
      );
      return;
    }

    // Ensure the test is meaningful: without a sandbox, these operations should succeed on normal
    // macOS hosts. If they don't, skip the test rather than producing misleading output.
    if std::fs::read("/etc/passwd").is_err() {
      eprintln!("skipping: cannot read /etc/passwd in parent process");
      return;
    }

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
      Ok(listener) => listener,
      Err(_) => {
        eprintln!("skipping: cannot bind localhost in parent process");
        return;
      }
    };
    let port = listener
      .local_addr()
      .expect("listener local addr")
      .port()
      .to_string();
    if TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).is_err() {
      eprintln!("skipping: cannot connect to localhost in parent process");
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos_spawn::tests::sandbox_exec_blocks_file_and_network";
    let args = vec![
      OsString::from("--exact"),
      OsString::from(test_name),
      OsString::from("--nocapture"),
    ];

    let mut cmd =
      sandbox_exec_command(&exe, &args).expect("construct sandbox-exec wrapped test command");
    cmd.env(CHILD_ENV, "1");
    cmd.env(PORT_ENV, port);
    let output = cmd.output().expect("spawn sandboxed child test process");
    assert!(
      output.status.success(),
      "sandboxed child should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
