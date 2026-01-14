//! Best-effort Linux namespace isolation.
//!
//! This is designed as an optional defense-in-depth layer for renderer processes:
//! - A new **network namespace** blocks all networking by default because the loopback interface
//!   starts down and no other interfaces exist.
//! - A new **mount namespace** (optional) can limit the blast radius of mount operations.

use crate::sandbox::SandboxStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxNamespacesConfig {
  /// When false, this module is a no-op and returns [`SandboxStatus::Unsupported`].
  pub enabled: bool,
  /// Attempt to create a separate mount namespace (`CLONE_NEWNS`) in addition to the network
  /// namespace.
  pub isolate_mount_namespace: bool,
}

impl Default for LinuxNamespacesConfig {
  fn default() -> Self {
    Self {
      enabled: false,
      isolate_mount_namespace: false,
    }
  }
}

/// Apply best-effort namespace isolation to the current process.
///
/// Returns [`SandboxStatus::Applied`] if a new network namespace was created successfully.
/// Returns [`SandboxStatus::Unsupported`] on platforms/hosts that disallow namespace creation
/// (e.g. missing kernel support, disabled user namespaces, or insufficient privileges).
pub fn apply_namespaces(config: LinuxNamespacesConfig) -> SandboxStatus {
  if !config.enabled {
    return SandboxStatus::Unsupported;
  }

  #[cfg(target_os = "linux")]
  {
    if !linux_try_unshare(libc::CLONE_NEWNET) {
      return SandboxStatus::Unsupported;
    }

    // Best-effort mount namespace isolation. This is optional because it can fail on hosts that
    // allow network namespaces but restrict mount namespaces (or vice versa).
    if config.isolate_mount_namespace {
      if linux_try_unshare(libc::CLONE_NEWNS) {
        // Prevent any future mount events from propagating back to the parent namespace. This is a
        // no-op if we never mount anything, but keeps the isolation safe if mount syscalls are
        // reachable.
        linux_try_make_mounts_private();
      }
    }

    // IMPORTANT: do not bring up any interfaces (including loopback). In a fresh net namespace the
    // loopback interface starts down, which provides the desired "no network" default.
    return SandboxStatus::Applied;
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = config;
    SandboxStatus::Unsupported
  }
}

#[cfg(target_os = "linux")]
fn linux_try_unshare(flags: libc::c_int) -> bool {
  // SAFETY: `unshare` is an OS syscall with no Rust-side invariants beyond passing a valid flag
  // mask.
  let rc = unsafe { libc::unshare(flags) };
  if rc == 0 {
    return true;
  }

  // Best-effort: most commonly EPERM (no permission) or EINVAL (unsupported / multithreaded
  // constraints). Either way, treat as unsupported rather than failing hard.
  let _err = std::io::Error::last_os_error();
  false
}

#[cfg(target_os = "linux")]
fn linux_try_make_mounts_private() {
  // Equivalent to: `mount --make-rprivate /`
  const ROOT_CSTR: &[u8] = b"/\0";
  // SAFETY: `ROOT_CSTR` is NUL-terminated. We pass null pointers where permitted by the syscall.
  let _ = unsafe {
    libc::mount(
      std::ptr::null(),
      ROOT_CSTR.as_ptr() as *const libc::c_char,
      std::ptr::null(),
      libc::MS_REC | libc::MS_PRIVATE,
      std::ptr::null(),
    )
  };
}
