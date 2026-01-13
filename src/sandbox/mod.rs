//! Renderer process sandboxing.
//!
//! This module is intended for **untrusted renderer processes** in the future multiprocess
//! architecture. The sandbox should be applied **as early as possible during renderer startup**,
//! and critically **before any thread pools spawn**.
//!
//! ## Platform implementations
//!
//! - **Linux**: `seccomp-bpf` with `SECCOMP_FILTER_FLAG_TSYNC` to ensure the filter is applied to
//!   all threads in the process. If you apply the sandbox after spawning threads, the kernel may
//!   reject the request (or you may inadvertently sandbox background threads that still need
//!   broader syscall access).
//! - **macOS**: renderers can call `sandbox_init(3)` (Seatbelt) in-process. For debugging/legacy,
//!   the browser process can also launch renderers through `/usr/bin/sandbox-exec` (deprecated by
//!   Apple; may be removed in future macOS releases). See [`macos_spawn`] for helpers and
//!   `FASTR_MACOS_USE_SANDBOX_EXEC=1` gating (ignored when sandboxing is disabled via
//!   `FASTR_DISABLE_RENDERER_SANDBOX=1` / `FASTR_MACOS_RENDERER_SANDBOX=off`).
//! - **Windows**: renderers are intended to be spawned in an AppContainer (no capabilities) with
//!   a Job Object configured for kill-on-close and active-process limiting. If AppContainer is
//!   unavailable, a restricted-token + low-integrity fallback is used (see [`windows`]).
//!
//! The current policy is intentionally small and focused:
//! - deny opening filesystem paths (e.g. `open/openat`)
//! - deny creating/using network sockets (e.g. `socket/connect`)
//! - deny executing new programs (`execve/execveat`)
//!
//! Additional restrictions can be layered over time (namespaces, Landlock, etc.).
//!
//! ## Inherited file descriptors
//!
//! Sandbox policies that block filesystem and network syscalls do not automatically revoke access
//! already granted via inherited file descriptors (e.g. a pre-opened file or socket). Use
//! [`close_fds_except`] as a defense-in-depth measure when spawning a sandboxed renderer to ensure
//! only explicitly-whitelisted FDs (stdio + IPC endpoints) remain open.

use std::io;

pub mod config;

pub mod fd_sanitizer;

pub mod spawn;

pub use fd_sanitizer::close_fds_except;

#[cfg(target_os = "linux")]
mod linux_prelude;

#[cfg(target_os = "linux")]
pub mod linux_landlock;

pub mod linux_namespaces;

// macOS Seatbelt sandbox support lives in `macos.rs`. Keep it behind a cfg so the crate still
// builds on non-macOS targets without linking against `libsandbox`.
#[cfg(target_os = "macos")]
pub mod macos;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
  Disabled,
  Applied,
  Unsupported,
}

/// Apply the macOS Seatbelt `pure-computation` sandbox to the current process.
///
/// This is primarily intended for sandboxing untrusted renderer subprocesses. It is a one-way
/// operation: once applied, the sandbox cannot be removed.
///
/// This is only supported on macOS (Seatbelt). On other platforms this returns
/// `io::ErrorKind::Unsupported`.
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::apply_pure_computation_sandbox();

  #[cfg(not(target_os = "macos"))]
  return Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "Seatbelt `pure-computation` sandboxing is only supported on macOS",
  ));
}

/// Network socket policy for sandboxed renderer processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
  /// Deny `socket(2)` / `socketpair(2)` entirely.
  ///
  /// This is the most conservative setting and is the default for renderer sandboxes unless a
  /// caller explicitly opts into Unix-domain sockets for IPC.
  DenyAllSockets,
  /// Allow Unix-domain sockets (`AF_UNIX`) while denying all other socket families.
  ///
  /// This enables IPC mechanisms that rely on `socketpair(AF_UNIX, ...)` or `socket(AF_UNIX, ...)`
  /// while still preventing direct access to the host network (AF_INET/AF_INET6/etc).
  ///
  /// Note: seccomp cannot reliably restrict `connect(2)`/`bind(2)` by inspecting the `sockaddr`
  /// pointer. The security model relies on denying non-Unix socket *creation* and ensuring the
  /// sandboxed process does not inherit any pre-existing network socket file descriptors.
  AllowUnixSocketsOnly,
}

impl Default for NetworkPolicy {
  fn default() -> Self {
    Self::DenyAllSockets
  }
}

/// Landlock sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererLandlockPolicy {
  /// Do not apply Landlock.
  Disabled,
  /// Best-effort Landlock that attempts to deny filesystem writes globally while still allowing
  /// reads (so dynamic linking continues to work).
  ///
  /// If the running kernel does not support Landlock, this falls back to a no-op.
  RestrictWrites,
}

/// Seccomp sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererSeccompPolicy {
  /// Do not install a seccomp filter.
  Disabled,
  /// Default renderer policy. Currently this blocks obvious "escape hatches" like creating
  /// network sockets.
  RendererDefault,
}

#[derive(Debug, Clone, Copy)]
pub struct RendererSandboxConfig {
  pub network_policy: NetworkPolicy,
  pub linux_namespaces: linux_namespaces::LinuxNamespacesConfig,
  /// Address space ceiling (RLIMIT_AS) in bytes. `None` disables the limit.
  pub address_space_limit_bytes: Option<u64>,
  /// File descriptor ceiling (RLIMIT_NOFILE). `None` disables the limit.
  pub nofile_limit: Option<u64>,
  /// Core dump size ceiling (RLIMIT_CORE) in bytes. `None` disables the limit.
  ///
  /// Renderer subprocesses should generally set this to `Some(0)` to ensure no coredumps are
  /// produced from untrusted content.
  pub core_limit_bytes: Option<u64>,
  /// Landlock filesystem policy.
  pub landlock: RendererLandlockPolicy,
  /// Seccomp policy.
  pub seccomp: RendererSeccompPolicy,
}

impl Default for RendererSandboxConfig {
  fn default() -> Self {
    Self {
      network_policy: NetworkPolicy::DenyAllSockets,
      linux_namespaces: linux_namespaces::LinuxNamespacesConfig::default(),
      address_space_limit_bytes: None,
      nofile_limit: None,
      core_limit_bytes: Some(0),
      landlock: RendererLandlockPolicy::Disabled,
      seccomp: RendererSeccompPolicy::RendererDefault,
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum RendererSandboxError {
  #[error("rlimit value {value} for {resource} does not fit platform rlim_t")]
  InvalidRlimitValue { resource: &'static str, value: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompInstallRejectedReason {
  /// The kernel rejected the request due to insufficient privileges (often: already sandboxed).
  PermissionDenied,
  /// The kernel rejected the request due to invalid arguments (often: already sandboxed, or
  /// unsupported flags like TSYNC on older kernels).
  InvalidArgument,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
  #[error("sandboxing is not supported on this platform")]
  UnsupportedPlatform,

  #[cfg(target_os = "linux")]
  #[error("failed to apply Landlock sandbox")]
  LandlockFailed {
    #[source]
    source: linux_landlock::LandlockError,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set parent-death signal via prctl(PR_SET_PDEATHSIG)")]
  SetParentDeathSignalFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set PR_SET_DUMPABLE=0")]
  SetDumpableFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set RLIMIT_CORE to 0")]
  DisableCoreDumpsFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to enable no_new_privs via prctl(PR_SET_NO_NEW_PRIVS)")]
  EnableNoNewPrivsFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error(
    "seccomp filter installation rejected ({reason:?}); process may already be sandboxed (errno {errno})"
  )]
  SeccompInstallRejected {
    reason: SeccompInstallRejectedReason,
    errno: i32,
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("seccomp filter installation failed (errno {errno})")]
  SeccompInstallFailed {
    errno: i32,
    #[source]
    source: io::Error,
  },

  #[error("invalid {var}: expected 0 or 1, got {value:?}")]
  InvalidBoolean0Or1 { var: &'static str, value: String },

  #[error(
    "invalid {var}: expected one of pure-computation, no-internet, renderer-default, or a path to an SBPL file; got {value:?}"
  )]
  InvalidMacosSeatbeltProfile { var: &'static str, value: String },

  #[error(
    "failed to read SBPL file {path:?} (from {var}={raw_value:?}); expected one of pure-computation, no-internet, renderer-default, or a path to an existing SBPL file"
  )]
  ReadSeatbeltProfileFailed {
    var: &'static str,
    raw_value: String,
    path: std::path::PathBuf,
    #[source]
    source: io::Error,
  },

  #[error("SBPL profile contains an interior NUL byte (from {var}={raw_value:?})")]
  SeatbeltProfileContainsNul { var: &'static str, raw_value: String },

  #[cfg(target_os = "macos")]
  #[error("failed to apply macOS Seatbelt sandbox (profile={profile}): {errorbuf}")]
  MacosSeatbeltInitFailed { profile: String, errorbuf: String },
}

/// Parse `FASTR_RENDERER_*` environment variables and apply the renderer sandbox when enabled.
///
/// This is intended to be called very early in renderer process startup (before spawning threads
/// or initializing libraries that might attempt privileged operations).
pub fn maybe_apply_renderer_sandbox_from_env() -> Result<SandboxStatus, SandboxError> {
  let default_enabled = cfg!(target_os = "macos");
  let config = match config::RendererSandboxEnvConfig::from_env(default_enabled) {
    Ok(config) => config,
    Err(err) => {
      eprintln!(
        "fastrender: renderer sandbox configuration error\n\
         error: {err}\n\
         hint: set FASTR_RENDERER_SANDBOX=0 to disable sandboxing for debugging"
      );
      return Err(err);
    }
  };

  if !config.enabled {
    return Ok(SandboxStatus::Disabled);
  }

  #[cfg(target_os = "macos")]
  {
    use config::MacosSeatbeltProfileSelection;
    let profile_desc = config.macos_seatbelt_profile.describe();

    let apply_result = match &config.macos_seatbelt_profile {
      MacosSeatbeltProfileSelection::PureComputation => macos::apply_renderer_sandbox(
        macos::MacosSandboxMode::PureComputation,
      ),
      MacosSeatbeltProfileSelection::NoInternet => macos::apply_named_profile("no-internet"),
      MacosSeatbeltProfileSelection::RendererDefault => macos::apply_renderer_sandbox(
        macos::MacosSandboxMode::RendererSystemFonts,
      ),
      MacosSeatbeltProfileSelection::SbplPath { .. } => {
        let sbpl = match config.macos_seatbelt_profile.load_sbpl_source() {
          Ok(sbpl) => sbpl,
          Err(err) => {
            eprintln!(
              "fastrender: failed to load macOS Seatbelt sandbox profile (profile={profile_desc})\n\
               error: {err}\n\
               hint: set FASTR_RENDERER_SANDBOX=0 to disable sandboxing for debugging"
            );
            return Err(err);
          }
        };
        macos::apply_profile_source_with_home_param(&sbpl)
      }
    };

    if let Err(err) = apply_result {
      // Log enough context for debugging/CI where the sandbox may be intentionally disabled.
      eprintln!(
        "fastrender: failed to apply macOS Seatbelt sandbox (profile={profile_desc})\n\
         errorbuf: {err}\n\
         hint: set FASTR_RENDERER_SANDBOX=0 to disable sandboxing for debugging"
      );
      return Err(SandboxError::MacosSeatbeltInitFailed {
        profile: profile_desc,
        errorbuf: err.to_string(),
      });
    }
    return Ok(SandboxStatus::Applied);
  }

  #[cfg(not(target_os = "macos"))]
  {
    Ok(SandboxStatus::Unsupported)
  }
}

/// Apply hardening steps that must run before seccomp is installed.
///
/// On Linux, this disables core dumps via `RLIMIT_CORE=0` and sets `PR_SET_DUMPABLE=0` so the
/// renderer does not leak sensitive data via core files.
#[cfg(target_os = "linux")]
pub fn apply_renderer_sandbox_prelude() -> Result<(), SandboxError> {
  linux_seccomp::apply_renderer_sandbox_prelude_linux()
}

/// On Linux, set `PR_SET_PDEATHSIG` so the current process is killed if its parent dies.
///
/// - Uses `SIGKILL` for reliability.
/// - Immediately checks `getppid()`: if it is `1`, the parent already died and the process exits.
///
/// On non-Linux platforms this is a no-op.
pub fn linux_set_parent_death_signal() -> io::Result<()> {
  #[cfg(target_os = "linux")]
  {
    return linux_prelude::linux_set_parent_death_signal();
  }

  #[cfg(not(target_os = "linux"))]
  {
    Ok(())
  }
}
/// Apply the renderer sandbox for the current process.
///
/// Call this during renderer startup, before spawning any thread pools. On Linux, the sandbox is
/// process-wide and uses `SECCOMP_FILTER_FLAG_TSYNC` to apply to all threads.
pub fn apply_renderer_sandbox(
  config: RendererSandboxConfig,
) -> Result<SandboxStatus, SandboxError> {
  #[cfg(target_os = "linux")]
  {
    // Best-effort defense-in-depth: isolate networking via a new network namespace when permitted.
    // This must run before seccomp, since the seccomp filter may block `unshare(2)`.
    let _ = linux_namespaces::apply_namespaces(config.linux_namespaces);

    // Apply Landlock as defense-in-depth. This doesn't affect already-open FDs (pipes, sockets,
    // memfd, etc.) because Landlock mediates path-based filesystem operations.
    match linux_landlock::apply(&linux_landlock::LandlockConfig::default()) {
      Ok(linux_landlock::LandlockStatus::Applied { .. }) => {}
      Ok(linux_landlock::LandlockStatus::Unsupported { .. }) => {}
      Err(source) => return Err(SandboxError::LandlockFailed { source }),
    }

    return linux_seccomp::apply_renderer_sandbox_linux(config);
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = config;
    Ok(SandboxStatus::Unsupported)
  }
}

/// Applies the Linux renderer seccomp denylist without additional sandbox layers.
///
/// This is primarily useful for unit tests and early bring-up of renderer processes where only
/// syscall filtering is desired.
pub fn apply_renderer_seccomp_denylist() -> Result<SandboxStatus, SandboxError> {
  #[cfg(target_os = "linux")]
  {
    return linux_seccomp::apply_renderer_sandbox_linux(RendererSandboxConfig::default());
  }

  #[cfg(not(target_os = "linux"))]
  {
    Ok(SandboxStatus::Unsupported)
  }
}

#[cfg(target_os = "linux")]
mod linux_seccomp;

#[cfg(target_os = "macos")]
pub mod macos_spawn;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::os::unix::net::UnixStream;
  use std::process::Command;

  fn is_seccomp_unsupported_error(err: &SandboxError) -> bool {
    let errno = match err {
      SandboxError::SetDumpableFailed { source }
      | SandboxError::DisableCoreDumpsFailed { source }
      | SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
      SandboxError::SeccompInstallRejected { errno, .. } => Some(*errno),
      SandboxError::SeccompInstallFailed { errno, .. } => Some(*errno),
      _ => None,
    };
    matches!(errno, Some(code) if code == libc::ENOSYS || code == libc::EINVAL)
  }

  #[test]
  fn renderer_seccomp_denylist_blocks_fs_and_network() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SECCOMP_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      match apply_renderer_seccomp_denylist() {
        Ok(SandboxStatus::Applied) => {}
        Ok(SandboxStatus::Disabled | SandboxStatus::Unsupported) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      // The sandbox is intended to be applied early, before thread pools spawn. Ensure basic
      // thread creation still works after seccomp is installed.
      let thread = std::thread::spawn(|| 1u32 + 1u32);
      assert_eq!(thread.join().expect("join thread"), 2);

      let fs_err = std::fs::read("/etc/passwd").expect_err("expected /etc/passwd read to fail");
      assert_eq!(
        fs_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for filesystem read (got {fs_err:?})"
      );

      let unix_err = UnixStream::pair().expect_err("expected UnixStream::pair to fail");
      assert_eq!(
        unix_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for Unix socketpair (got {unix_err:?})"
      );

      let net_err =
        TcpListener::bind("127.0.0.1:0").expect_err("expected bind to fail");
      assert_eq!(
        net_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for network bind (got {net_err:?})"
      );

      // Optional sanity check: process execution should be blocked (`execve`).
      let err = Command::new("/bin/true")
        .status()
        .expect_err("expected exec to be blocked under seccomp");
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected exec to fail with EPERM (got {err:?})"
      );

      return;
    }

    // Run the sandbox assertions in a child process so the parent test runner is unaffected.
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::tests::renderer_seccomp_denylist_blocks_fs_and_network";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      // Avoid a large libtest threadpool: the sandbox uses TSYNC and applies to all threads.
      .env("RUST_TEST_THREADS", "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
  #[test]
  fn renderer_seccomp_allow_unix_sockets_only_allows_unix_ipc_but_denies_tcp() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SECCOMP_UNIX_ONLY_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      match linux_seccomp::apply_renderer_sandbox_linux(RendererSandboxConfig {
        network_policy: NetworkPolicy::AllowUnixSocketsOnly,
        ..Default::default()
      }) {
        Ok(SandboxStatus::Applied) => {}
        Ok(SandboxStatus::Disabled | SandboxStatus::Unsupported) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      let (mut a, mut b) = UnixStream::pair().expect("expected UnixStream::pair to succeed");
      a.write_all(b"ping").expect("write to unix socket");
      let mut buf = [0u8; 4];
      b.read_exact(&mut buf).expect("read from unix socket");
      assert_eq!(&buf, b"ping");

      let net_err = TcpListener::bind("127.0.0.1:0").expect_err("expected tcp bind to fail");
      assert_eq!(
        net_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for network bind (got {net_err:?})"
      );

      return;
    }

    // Run the sandbox assertions in a child process so the parent test runner is unaffected.
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::tests::renderer_seccomp_allow_unix_sockets_only_allows_unix_ipc_but_denies_tcp";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env("RUST_TEST_THREADS", "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
#[cfg(all(test, target_os = "macos"))]
mod tests {
  #[cfg(target_os = "macos")]
  mod macos {
    use super::super::apply_pure_computation_sandbox;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn pure_computation_sandbox_allows_inherited_stdout_pipe() {
      const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_STDOUT_CHILD";
      const SENTINEL: &[u8] = b"fastrender-seatbelt-stdout-ok";

      if std::env::var_os(CHILD_ENV).is_some() {
        apply_pure_computation_sandbox().expect("apply Seatbelt pure-computation sandbox");
        std::io::stdout()
          .write_all(SENTINEL)
          .and_then(|_| std::io::stdout().flush())
          .expect("write sentinel to stdout after sandbox");
        std::process::exit(0);
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::pure_computation_sandbox_allows_inherited_stdout_pipe";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox child process");

      assert!(
        output.status.success(),
        "sandbox child should exit 0 (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );

      assert!(
        output
          .stdout
          .windows(SENTINEL.len())
          .any(|window| window == SENTINEL),
        "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={} ",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }
}
