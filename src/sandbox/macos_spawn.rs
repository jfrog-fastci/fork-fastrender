//! macOS renderer-launch sandboxing helpers.
//!
//! # `sandbox-exec` vs `sandbox_init`
//!
//! macOS exposes the "Seatbelt" sandbox through two main interfaces:
//!
//! - **`/usr/bin/sandbox-exec` (debug/legacy, deprecated by Apple):**
//!   The browser process launches the renderer *through* `sandbox-exec`, so the renderer starts
//!   executing already sandboxed. This is useful when the parent (browser) process is
//!   multithreaded and cannot safely use `std::os::unix::process::CommandExt::pre_exec`, which runs
//!   after `fork()` and is undefined/unsafe in a multithreaded parent.
//!
//!   ⚠️ Apple has deprecated `sandbox-exec` and may remove it in future macOS releases. FastRender
//!   keeps this path primarily as a pragmatic fallback for debugging or for experimenting with SBPL
//!   profiles externally. Prefer in-process `sandbox_init` for long-term sandboxing.
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
//!
//! # Opt-in gate
//!
//! `sandbox-exec` usage is intentionally opt-in. Set:
//!
//! ```text
//! FASTR_MACOS_USE_SANDBOX_EXEC=1
//! ```
//!
//! And then call [`maybe_wrap_command_with_sandbox_exec`] (or
//! [`wrap_command_with_sandbox_exec`]) when spawning a renderer.

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

/// Opt-in env var gate for wrapping child processes with `sandbox-exec`.
#[cfg(target_os = "macos")]
pub const ENV_MACOS_USE_SANDBOX_EXEC: &str = "FASTR_MACOS_USE_SANDBOX_EXEC";

#[cfg(target_os = "macos")]
fn parse_env_bool(raw: Option<&str>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return false;
  }
  !matches!(
    raw.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

/// Returns `true` when `FASTR_MACOS_USE_SANDBOX_EXEC` is set to an enabled value.
#[cfg(target_os = "macos")]
pub fn macos_use_sandbox_exec_from_env() -> bool {
  parse_env_bool(std::env::var(ENV_MACOS_USE_SANDBOX_EXEC).ok().as_deref())
}

/// Conditionally wrap `cmd` under `sandbox-exec` when `FASTR_MACOS_USE_SANDBOX_EXEC` is enabled.
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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

  let mut wrapped = Command::new(SANDBOX_EXEC_PATH);
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
        let err = result.expect_err("expected network bind to be blocked by sandbox-exec");
        assert!(
          err.kind() == io::ErrorKind::PermissionDenied
            || matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)),
          "expected network bind to be denied by sandbox-exec, got {err:?}"
        );
      }
      return;
    }

    let _guard = env_lock().lock().unwrap();

    // Skip the test if sandbox-exec is missing (it is deprecated and may not exist on future macOS
    // releases / minimal images).
    if !Path::new(SANDBOX_EXEC_PATH).is_file() {
      eprintln!("skipping: {SANDBOX_EXEC_PATH} is missing");
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::macos_spawn::tests::sandbox_exec_blocks_network_bind";

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
