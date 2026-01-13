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
//! Note: this is ignored when sandboxing is disabled via `FASTR_DISABLE_RENDERER_SANDBOX=1`,
//! `FASTR_RENDERER_SANDBOX=off`, or `FASTR_MACOS_RENDERER_SANDBOX=off` (debug escape hatch; insecure).
//! 
//! And then call [`maybe_wrap_command_with_sandbox_exec`] (or
//! [`wrap_command_with_sandbox_exec`]) when spawning a renderer.

#[cfg(target_os = "macos")]
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
#[cfg(target_os = "macos")]
use std::{fs, io};

#[cfg(target_os = "macos")]
use crate::sandbox::config;
#[cfg(target_os = "macos")]
use crate::sandbox::macos::{ENV_DISABLE_RENDERER_SANDBOX, ENV_MACOS_RENDERER_SANDBOX};
#[cfg(target_os = "macos")]
use crate::sandbox_exec::{SandboxExecCommand, SandboxExecProfile, SeatbeltParameters};

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

#[cfg(target_os = "macos")]
fn renderer_sandbox_disabled_via_env() -> bool {
  if parse_env_bool(std::env::var(ENV_DISABLE_RENDERER_SANDBOX).ok().as_deref()) {
    return true;
  }

  // New multiprocess renderer sandbox knob. Keep the `sandbox-exec` helpers in sync so
  // `FASTR_RENDERER_SANDBOX=off` disables sandboxing regardless of whether the renderer applies the
  // sandbox in-process (`sandbox_init`) or is launched already sandboxed (`sandbox-exec`).
  if let Ok(raw) = std::env::var(config::ENV_RENDERER_SANDBOX) {
    let raw = raw.trim();
    if !raw.is_empty() && matches!(raw.to_ascii_lowercase().as_str(), "0" | "off") {
      return true;
    }
  }

  let Ok(raw) = std::env::var(ENV_MACOS_RENDERER_SANDBOX) else {
    return false;
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return false;
  }
  matches!(
    raw.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

#[cfg(target_os = "macos")]
fn log_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: macOS Seatbelt renderer sandbox is DISABLED (debug escape hatch; INSECURE). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1, {config}={strict}|{relaxed}|{off}, or {ENV_MACOS_RENDERER_SANDBOX}=pure-computation|system-fonts|off to control this.",
      config = config::ENV_RENDERER_SANDBOX,
      strict = "strict",
      relaxed = "relaxed",
      off = "off",
    );
  });
}

/// Returns `true` when `FASTR_MACOS_USE_SANDBOX_EXEC` is set to an enabled value.
#[cfg(target_os = "macos")]
pub fn macos_use_sandbox_exec_from_env() -> bool {
  if renderer_sandbox_disabled_via_env() {
    return false;
  }
  parse_env_bool(std::env::var(ENV_MACOS_USE_SANDBOX_EXEC).ok().as_deref())
}

/// Conditionally wrap `cmd` under `sandbox-exec` when `FASTR_MACOS_USE_SANDBOX_EXEC` is enabled.
#[cfg(target_os = "macos")]
pub fn maybe_wrap_command_with_sandbox_exec(
  cmd: &Command,
  sbpl: &str,
) -> io::Result<Option<SandboxExecCommand>> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(None);
  }
  if !macos_use_sandbox_exec_from_env() {
    return Ok(None);
  }
  wrap_command_with_sandbox_exec(cmd, sbpl)
}

/// Wrap `cmd` so it executes under `sandbox-exec`.
///
/// Specifically, the command is transformed to:
/// `sandbox-exec -f <temp_profile_file> -- <original-exe> <args...>`
///
/// The profile string is written to a temporary file (mode `0600`). The file is removed
/// immediately after spawning the sandboxed child process (best-effort).
///
/// The wrapper also defines a small set of common Seatbelt parameters via `sandbox-exec -D`, so SBPL
/// profiles can use `(param "HOME")` / `(param "TMPDIR")` consistently:
/// `sandbox-exec -D HOME=... -D TMPDIR=... -f <profile_file> -- <original-exe> <args...>`
///
/// This helper does **not** invoke a shell; all arguments are forwarded as separate argv entries.
///
/// Note: The wrapper constructs a new `sandbox-exec` command internally. Environment overrides and
/// `current_dir` are preserved, but other configuration (notably stdio) should be applied to the
/// returned command after calling this helper.
///
/// Returns `Ok(None)` when the renderer sandbox is disabled via the debug escape-hatch env vars.
#[cfg(target_os = "macos")]
pub fn wrap_command_with_sandbox_exec(
  cmd: &Command,
  sbpl: &str,
) -> io::Result<Option<SandboxExecCommand>> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(None);
  }
  if !Path::new(SANDBOX_EXEC_PATH).is_file() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!("missing {}", SANDBOX_EXEC_PATH),
    ));
  }
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

  // Pass HOME/TMPDIR as Seatbelt parameters so SBPL profiles can reference them via `(param "HOME")`
  // and `(param "TMPDIR")`.
  //
  // This matches the in-process Seatbelt wrapper (`sandbox_init_with_parameters`) behaviour in
  // `src/sandbox/macos.rs`, and keeps SBPL profiles stable whether they are applied in-process or
  // via `sandbox-exec`.
  let mut home = std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/Users"));
  if home.is_empty() {
    home = OsString::from("/Users");
  }
  let mut tmpdir =
    std::env::var_os("TMPDIR").unwrap_or_else(|| std::env::temp_dir().into_os_string());
  if tmpdir.is_empty() {
    tmpdir = std::env::temp_dir().into_os_string();
  }
  for (key, value) in cmd.get_envs() {
    if key == OsStr::new("HOME") {
      home = match value {
        Some(v) if !v.is_empty() => v.to_os_string(),
        _ => OsString::from("/Users"),
      };
    } else if key == OsStr::new("TMPDIR") {
      tmpdir = match value {
        Some(v) if !v.is_empty() => v.to_os_string(),
        _ => std::env::temp_dir().into_os_string(),
      };
    }
  }

  let original_program = cmd.get_program();
  if original_program.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "command program is empty",
    ));
  }
  let current_dir: Option<PathBuf> = cmd.get_current_dir().map(PathBuf::from);

  let params = SeatbeltParameters::new(
    home.to_string_lossy().into_owned(),
    tmpdir.to_string_lossy().into_owned(),
  );
  let mut wrapped = SandboxExecCommand::new_with_parameters(
    SandboxExecProfile::Custom(sbpl.to_string()),
    params,
    original_program,
    cmd.get_args(),
  )?;

  if let Some(dir) = current_dir {
    wrapped.command_mut().current_dir(dir);
  }
  for (key, value) in cmd.get_envs() {
    match value {
      Some(value) => {
        wrapped.command_mut().env(key, value);
      }
      None => {
        wrapped.command_mut().env_remove(key);
      }
    }
  }
  Ok(Some(wrapped))
}

/// Fallback sandbox profile used with `sandbox-exec -f ...` when `sandbox-exec -n` is unavailable.
///
/// The goal is to be "equivalent enough" to the built-in `pure-computation` profile for our use
/// case: deny filesystem access (outside of system dynamic-loader locations) and deny all network.
///
/// Notes:
/// - We intentionally do **not** whitelist `/private/etc` (where `/etc` resolves), so attempts to
///   read `/etc/passwd` should be denied.
/// - This profile still allows process execution (`process*`) so the target renderer can start.
#[cfg(target_os = "macos")]
const PURE_COMPUTATION_PROFILE_FALLBACK: &str =
  crate::sandbox::macos::RELAXED_SYSTEM_ALLOWLIST_PROFILE;

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
  #[error("failed to build sandbox-exec command")]
  BuildCommandFailed {
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
  INVOCATION.get().ok_or_else(|| SandboxExecError::ProbeFailed {
    source: io::Error::new(
      io::ErrorKind::Other,
      "sandbox-exec invocation missing after initialization",
    ),
  })
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
  // In either case, fall back to a custom profile string written to a temp file (`-f`).
  let probe = Command::new(SANDBOX_EXEC_PATH)
    .arg("-n")
    .arg(PURE_COMPUTATION_PROFILE_NAME)
    .arg("--")
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

/// Build a [`SandboxExecCommand`] that launches `renderer_path` through `/usr/bin/sandbox-exec`.
///
/// This is intended for the "browser" (trusted) process to spawn the "renderer" (untrusted)
/// process already sandboxed **without** relying on `CommandExt::pre_exec`.
///
/// The returned command wrapper can be further configured by the caller (environment variables, stdio,
/// working directory, etc). Those settings apply to `sandbox-exec` and are inherited by the
/// renderer it `exec`s.
#[cfg(target_os = "macos")]
pub fn sandbox_exec_command(
  renderer_path: &Path,
  args: &[OsString],
) -> Result<Option<SandboxExecCommand>, SandboxExecError> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return Ok(None);
  }
  let invocation = sandbox_exec_invocation()?;

  let profile = match invocation {
    SandboxExecInvocation::NamedProfile => {
      SandboxExecProfile::Named(OsString::from(PURE_COMPUTATION_PROFILE_NAME))
    }
    SandboxExecInvocation::InlineProfile { profile } => SandboxExecProfile::Custom(profile.clone()),
  };

  SandboxExecCommand::new(profile, renderer_path.as_os_str(), args.iter())
    .map(Some)
    .map_err(|source| SandboxExecError::BuildCommandFailed { source })
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
  fn wrap_command_is_noop_when_renderer_sandbox_disabled() {
    let _guard = env_lock().lock().unwrap();
    let prev = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");

    let mut cmd = Command::new("/usr/bin/true");
    cmd.arg("--hello");

    // Even though the SBPL is invalid, the escape hatch should bypass rewriting entirely.
    let wrapped = wrap_command_with_sandbox_exec(&cmd, "")
      .expect("wrap should not error when disabled");
    assert!(wrapped.is_none(), "wrap should be a no-op when disabled");

    assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/true"));
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, vec![OsStr::new("--hello")]);

    match prev {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
  }

  #[test]
  fn maybe_wrap_is_noop_when_renderer_sandbox_disabled_even_if_env_gate_enabled() {
    let _guard = env_lock().lock().unwrap();
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_gate = std::env::var_os(ENV_MACOS_USE_SANDBOX_EXEC);
    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");
    std::env::set_var(ENV_MACOS_USE_SANDBOX_EXEC, "1");

    let mut cmd = Command::new("/usr/bin/true");
    cmd.arg("--hello");

    // Even though the env gate is enabled, disabling the renderer sandbox should bypass wrapping.
    // The SBPL is intentionally invalid; if wrapping happened we would get an error.
    let wrapped = maybe_wrap_command_with_sandbox_exec(&cmd, "")
      .expect("maybe_wrap should be a no-op when sandbox is disabled");
    assert!(wrapped.is_none(), "maybe_wrap should not wrap when sandbox is disabled");
    assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/true"));
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, vec![OsStr::new("--hello")]);

    match prev_gate {
      Some(value) => std::env::set_var(ENV_MACOS_USE_SANDBOX_EXEC, value),
      None => std::env::remove_var(ENV_MACOS_USE_SANDBOX_EXEC),
    }
    match prev_disable {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
  }

  #[test]
  fn sandbox_exec_command_is_none_when_renderer_sandbox_disabled() {
    let _guard = env_lock().lock().unwrap();
    let prev = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");

    let cmd = sandbox_exec_command(Path::new("/usr/bin/true"), &[])
      .expect("sandbox_exec_command should be a no-op when disabled");
    assert!(cmd.is_none(), "expected no sandbox-exec command when disabled");

    match prev {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
  }

  #[test]
  fn wrap_command_is_noop_when_renderer_sandbox_off_via_fastr_renderer_sandbox() {
    let _guard = env_lock().lock().unwrap();
    let prev = std::env::var_os(crate::sandbox::config::ENV_RENDERER_SANDBOX);
    std::env::set_var(crate::sandbox::config::ENV_RENDERER_SANDBOX, "off");

    let mut cmd = Command::new("/usr/bin/true");
    cmd.arg("--hello");

    // Even though the SBPL is invalid, the escape hatch should bypass rewriting entirely.
    let wrapped =
      wrap_command_with_sandbox_exec(&cmd, "").expect("wrap should not error when disabled");
    assert!(wrapped.is_none(), "expected wrap to be a no-op when disabled");

    assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/true"));
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, vec![OsStr::new("--hello")]);

    match prev {
      Some(value) => std::env::set_var(crate::sandbox::config::ENV_RENDERER_SANDBOX, value),
      None => std::env::remove_var(crate::sandbox::config::ENV_RENDERER_SANDBOX),
    }
  }

  #[test]
  fn wrap_command_with_sandbox_exec_rewrites_argv_including_params() {
    if !Path::new(SANDBOX_EXEC_PATH).is_file() {
      eprintln!("skipping: {SANDBOX_EXEC_PATH} is missing");
      return;
    }

    let sbpl = "(version 1)\n(allow default)\n";
    let home = "/Users/Test User";
    let tmpdir = "/var/folders/xx/Some Tmp";

    let mut cmd = Command::new("/usr/bin/true");
    cmd.env("HOME", home).env("TMPDIR", tmpdir);
    let wrapped = wrap_command_with_sandbox_exec(&cmd, sbpl)
      .expect("wrap command")
      .expect("expected wrapper");

    assert_eq!(wrapped.get_program(), OsStr::new(SANDBOX_EXEC_PATH));
    let args: Vec<String> = wrapped
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect();

    // argv should be:
    // sandbox-exec -D HOME=... -D TMPDIR=... -f <profile_file> -- /usr/bin/true
    assert!(
      args.len() >= 8,
      "expected at least 8 argv entries, got {args:?}"
    );
    assert_eq!(args[0], "-D");
    assert_eq!(args[1], format!("HOME={home}"));
    assert_eq!(args[2], "-D");
    assert_eq!(args[3], format!("TMPDIR={tmpdir}"));
    assert_eq!(args[4], "-f");
    assert!(
      Path::new(&args[5]).is_file(),
      "expected profile path to exist, got {:?}",
      args[5]
    );
    let profile_contents =
      std::fs::read_to_string(&args[5]).expect("read temp profile file");
    assert_eq!(profile_contents, sbpl);
    assert_eq!(args[6], "--");
    assert_eq!(args[7], "/usr/bin/true");
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
    let mut wrapped = wrap_command_with_sandbox_exec(&cmd, sbpl)
      .expect("wrap command with sandbox-exec")
      .expect("expected wrapper");
    let output = wrapped.output().expect("spawn sandboxed child process");
    assert!(
      output.status.success(),
      "sandboxed child should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
