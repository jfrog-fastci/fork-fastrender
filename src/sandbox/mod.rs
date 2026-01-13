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
//! - **macOS**: renderers can call `sandbox_init(3)` (Seatbelt) in-process, but the browser process
//!   may also want a *pre-main* sandbox when spawning renderers from a multithreaded parent. See
//!   [`macos_spawn`] for a `/usr/bin/sandbox-exec` based launcher helper.
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

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
use std::io;

#[cfg(target_os = "linux")]
pub mod linux_landlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
  Applied,
  Unsupported,
}

#[derive(Debug, Clone, Copy)]
pub struct RendererSandboxConfig {
  // Reserved for future policy knobs.
}

impl Default for RendererSandboxConfig {
  fn default() -> Self {
    Self {}
  }
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
}

/// Apply the macOS Seatbelt `pure-computation` sandbox to the current process.
///
/// This is intended for sandboxing untrusted renderer subprocesses. It is a one-way operation:
/// once applied, the sandbox cannot be removed.
pub fn apply_pure_computation_sandbox() -> std::io::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::apply_pure_computation_sandbox();

  #[cfg(not(target_os = "macos"))]
  return Err(std::io::Error::new(
    std::io::ErrorKind::Unsupported,
    "Seatbelt sandboxing is only supported on macOS",
  ));
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

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use std::process::Command;

  fn is_seccomp_unsupported_error(err: &SandboxError) -> bool {
    let errno = match err {
      SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
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
        Ok(SandboxStatus::Unsupported) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      let fs_err = std::fs::read("/etc/passwd").expect_err("expected /etc/passwd read to fail");
      assert_eq!(
        fs_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for filesystem read (got {fs_err:?})"
      );

      let net_err =
        std::net::TcpListener::bind("127.0.0.1:0").expect_err("expected bind to fail");
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
}
