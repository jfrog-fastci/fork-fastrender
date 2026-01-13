//! Renderer process sandboxing.
//!
//! This module is intended for **untrusted renderer processes** in the future multiprocess
//! architecture. The sandbox should be applied **as early as possible during renderer startup**,
//! and critically **before any thread pools spawn**.
//!
//! On Linux, the implementation uses `seccomp-bpf` with `SECCOMP_FILTER_FLAG_TSYNC` to ensure the
//! filter is applied to all threads in the process. If you apply the sandbox after spawning
//! threads, the kernel may reject the request (or you may inadvertently sandbox background
//! threads that still need broader syscall access).
//!
//! The current policy is intentionally small and focused:
//! - deny opening filesystem paths (e.g. `open/openat`)
//! - deny creating/using network sockets (e.g. `socket/connect`)
//! - deny executing new programs (`execve/execveat`)
//!
//! Additional restrictions can be layered over time (namespaces, Landlock, etc.).

#[cfg(target_os = "linux")]
use std::io;

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

/// Apply the renderer sandbox for the current process.
///
/// Call this during renderer startup, before spawning any thread pools. On Linux, the sandbox is
/// process-wide and uses `SECCOMP_FILTER_FLAG_TSYNC` to apply to all threads.
pub fn apply_renderer_sandbox(
  config: RendererSandboxConfig,
) -> Result<SandboxStatus, SandboxError> {
  #[cfg(target_os = "linux")]
  {
    return linux_seccomp::apply_renderer_sandbox_linux(config);
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = config;
    return Ok(SandboxStatus::Unsupported);
  }
}

#[cfg(target_os = "linux")]
mod linux_seccomp;

#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use std::ffi::CString;
  use std::process::Command;

  #[test]
  fn renderer_sandbox_blocks_fs_network_and_exec() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SANDBOX_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let status =
        apply_renderer_sandbox(RendererSandboxConfig::default()).expect("apply sandbox in child");
      assert_eq!(status, SandboxStatus::Applied);

      // Filesystem: `open("/etc/passwd")` should be blocked.
      let path = CString::new("/etc/passwd").expect("cstr");
      // SAFETY: `open` is called with a valid C string.
      let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
      assert_eq!(fd, -1, "expected open to fail under seccomp");
      let err = std::io::Error::last_os_error();
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected open to fail with EPERM (got {err:?})"
      );

      // Network: `socket(AF_INET, ...)` should be blocked.
      // SAFETY: `socket` is a raw libc call.
      let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
      assert_eq!(sock, -1, "expected socket to fail under seccomp");
      let err = std::io::Error::last_os_error();
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected socket to fail with EPERM (got {err:?})"
      );

      // Process execution: spawning a binary should fail because `execve` is blocked.
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
    let test_name = "sandbox::tests::renderer_sandbox_blocks_fs_network_and_exec";
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

#[cfg(target_os = "macos")]
pub mod macos;
