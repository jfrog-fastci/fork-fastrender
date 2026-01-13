//! Optional Landlock filesystem sandbox (Linux-only).
//!
//! Landlock complements syscall filtering (e.g. seccomp-bpf) by providing a path-based access
//! control layer. Even if a syscall slips through a seccomp filter, Landlock can still deny
//! filesystem access.
//!
//! This implementation intentionally uses direct syscalls (via `libc::syscall`) and local uapi
//! constants/structs to avoid pulling in additional dependencies.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockUnsupportedReason {
  /// The running kernel does not implement Landlock (`ENOSYS`).
  KernelUnsupported,
  /// This build does not know the Landlock syscall numbers for the current architecture.
  UnknownArchitecture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockStatus {
  Applied { abi: u32 },
  Unsupported { reason: LandlockUnsupportedReason },
}

#[derive(Debug, thiserror::Error)]
pub enum LandlockError {
  #[error("failed to probe Landlock ABI version")]
  ProbeFailed {
    #[source]
    source: io::Error,
  },
  #[error("failed to create Landlock ruleset")]
  CreateRulesetFailed {
    #[source]
    source: io::Error,
  },
  #[error("failed to open path {path:?} for Landlock ruleset")]
  OpenPathFailed {
    path: PathBuf,
    #[source]
    source: io::Error,
  },
  #[error("failed to add Landlock rule for path {path:?}")]
  AddRuleFailed {
    path: PathBuf,
    #[source]
    source: io::Error,
  },
  #[error("failed to set no_new_privs via prctl(PR_SET_NO_NEW_PRIVS)")]
  SetNoNewPrivsFailed {
    #[source]
    source: io::Error,
  },
  #[error("failed to restrict self with Landlock")]
  RestrictSelfFailed {
    #[source]
    source: io::Error,
  },
}

// --- Landlock uapi ------------------------------------------------------------

// Landlock syscall numbers.
//
// These are stable per-architecture, but `libc` may not expose them on older toolchains. We only
// define the subset of architectures used by our CI/agents; other architectures fall back to
// treating Landlock as unsupported.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 0;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 0;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 0;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
const LANDLOCK_ARCH_SUPPORTED: bool = false;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
const LANDLOCK_ARCH_SUPPORTED: bool = true;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

// Access rights (filesystem).
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
// Added with ABI v2.
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
// Added with ABI v3.
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;

const HANDLED_FS_ABI_V1: u64 = LANDLOCK_ACCESS_FS_EXECUTE
  | LANDLOCK_ACCESS_FS_WRITE_FILE
  | LANDLOCK_ACCESS_FS_READ_FILE
  | LANDLOCK_ACCESS_FS_READ_DIR
  | LANDLOCK_ACCESS_FS_REMOVE_DIR
  | LANDLOCK_ACCESS_FS_REMOVE_FILE
  | LANDLOCK_ACCESS_FS_MAKE_CHAR
  | LANDLOCK_ACCESS_FS_MAKE_DIR
  | LANDLOCK_ACCESS_FS_MAKE_REG
  | LANDLOCK_ACCESS_FS_MAKE_SOCK
  | LANDLOCK_ACCESS_FS_MAKE_FIFO
  | LANDLOCK_ACCESS_FS_MAKE_BLOCK
  | LANDLOCK_ACCESS_FS_MAKE_SYM;
const HANDLED_FS_ABI_V2: u64 = HANDLED_FS_ABI_V1 | LANDLOCK_ACCESS_FS_REFER;
const HANDLED_FS_ABI_V3: u64 = HANDLED_FS_ABI_V2 | LANDLOCK_ACCESS_FS_TRUNCATE;

fn handled_access_fs_for_abi(abi: u32) -> u64 {
  match abi {
    0 => 0,
    1 => HANDLED_FS_ABI_V1,
    2 => HANDLED_FS_ABI_V2,
    _ => HANDLED_FS_ABI_V3,
  }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LandlockRulesetAttr {
  handled_access_fs: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct LandlockPathBeneathAttr {
  allowed_access: u64,
  parent_fd: i32,
  _reserved: u32,
}

// --- Public API ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandlockAccess(u64);

impl LandlockAccess {
  pub const EMPTY: Self = Self(0);

  pub const EXECUTE: Self = Self(LANDLOCK_ACCESS_FS_EXECUTE);
  pub const WRITE_FILE: Self = Self(LANDLOCK_ACCESS_FS_WRITE_FILE);
  pub const READ_FILE: Self = Self(LANDLOCK_ACCESS_FS_READ_FILE);
  pub const READ_DIR: Self = Self(LANDLOCK_ACCESS_FS_READ_DIR);
  pub const REMOVE_DIR: Self = Self(LANDLOCK_ACCESS_FS_REMOVE_DIR);
  pub const REMOVE_FILE: Self = Self(LANDLOCK_ACCESS_FS_REMOVE_FILE);
  pub const MAKE_CHAR: Self = Self(LANDLOCK_ACCESS_FS_MAKE_CHAR);
  pub const MAKE_DIR: Self = Self(LANDLOCK_ACCESS_FS_MAKE_DIR);
  pub const MAKE_REG: Self = Self(LANDLOCK_ACCESS_FS_MAKE_REG);
  pub const MAKE_SOCK: Self = Self(LANDLOCK_ACCESS_FS_MAKE_SOCK);
  pub const MAKE_FIFO: Self = Self(LANDLOCK_ACCESS_FS_MAKE_FIFO);
  pub const MAKE_BLOCK: Self = Self(LANDLOCK_ACCESS_FS_MAKE_BLOCK);
  pub const MAKE_SYM: Self = Self(LANDLOCK_ACCESS_FS_MAKE_SYM);
  pub const REFER: Self = Self(LANDLOCK_ACCESS_FS_REFER);
  pub const TRUNCATE: Self = Self(LANDLOCK_ACCESS_FS_TRUNCATE);

  pub const fn bits(self) -> u64 {
    self.0
  }

  pub const fn is_empty(self) -> bool {
    self.0 == 0
  }

  pub const fn union(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }

  pub const fn intersection(self, other: Self) -> Self {
    Self(self.0 & other.0)
  }
}

impl std::ops::BitOr for LandlockAccess {
  type Output = Self;
  fn bitor(self, rhs: Self) -> Self::Output {
    self.union(rhs)
  }
}

impl std::ops::BitOrAssign for LandlockAccess {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

#[derive(Debug, Clone)]
pub struct LandlockConfig {
  /// List of filesystem paths that should be accessible, along with the access rights granted
  /// within that subtree.
  pub allowed_paths: Vec<(PathBuf, LandlockAccess)>,
}

impl LandlockConfig {
  pub fn deny_all() -> Self {
    Self {
      allowed_paths: Vec::new(),
    }
  }
}

impl Default for LandlockConfig {
  fn default() -> Self {
    Self::deny_all()
  }
}

/// Apply a Landlock ruleset to the current thread (and therefore the process when done early).
///
/// When Landlock is unsupported, this returns [`LandlockStatus::Unsupported`] (best-effort).
pub fn apply(config: &LandlockConfig) -> Result<LandlockStatus, LandlockError> {
  if !LANDLOCK_ARCH_SUPPORTED {
    return Ok(LandlockStatus::Unsupported {
      reason: LandlockUnsupportedReason::UnknownArchitecture,
    });
  }

  let abi_version = match probe_abi_version() {
    Ok(version) => version,
    Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => {
      return Ok(LandlockStatus::Unsupported {
        reason: LandlockUnsupportedReason::KernelUnsupported,
      });
    }
    // Landlock syscalls may exist but the Landlock LSM can be disabled at runtime (e.g. missing
    // from the kernel's `lsm=` list). In that case the kernel returns EOPNOTSUPP.
    Err(err) if err.raw_os_error() == Some(libc::EOPNOTSUPP) => {
      return Ok(LandlockStatus::Unsupported {
        reason: LandlockUnsupportedReason::KernelUnsupported,
      });
    }
    Err(err) => return Err(LandlockError::ProbeFailed { source: err }),
  };
  if abi_version == 0 {
    return Ok(LandlockStatus::Unsupported {
      reason: LandlockUnsupportedReason::KernelUnsupported,
    });
  }

  let handled_access_fs = handled_access_fs_for_abi(abi_version);
  let ruleset_attr = LandlockRulesetAttr { handled_access_fs };
  let ruleset_fd = match landlock_create_ruleset(&ruleset_attr) {
    Ok(fd) => fd,
    Err(err) if err.raw_os_error() == Some(libc::ENOSYS) || err.raw_os_error() == Some(libc::EOPNOTSUPP) => {
      return Ok(LandlockStatus::Unsupported {
        reason: LandlockUnsupportedReason::KernelUnsupported,
      });
    }
    Err(source) => {
      return Err(LandlockError::CreateRulesetFailed {
        source,
      });
    }
  };

  for (path, access) in &config.allowed_paths {
    add_path_rule(ruleset_fd.as_raw_fd(), path, *access, handled_access_fs)?;
  }

  // Landlock requires no_new_privs.
  // SAFETY: `prctl` with PR_SET_NO_NEW_PRIVS is process-scoped; argument types match the syscall.
  let prctl_rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  if prctl_rc != 0 {
    return Err(LandlockError::SetNoNewPrivsFailed {
      source: io::Error::last_os_error(),
    });
  }

  if let Err(err) = landlock_restrict_self(ruleset_fd.as_raw_fd()) {
    if err.raw_os_error() == Some(libc::ENOSYS) || err.raw_os_error() == Some(libc::EOPNOTSUPP) {
      return Ok(LandlockStatus::Unsupported {
        reason: LandlockUnsupportedReason::KernelUnsupported,
      });
    }
    return Err(LandlockError::RestrictSelfFailed { source: err });
  }

  Ok(LandlockStatus::Applied { abi: abi_version })
}

// --- Syscall wrappers ---------------------------------------------------------

fn probe_abi_version() -> io::Result<u32> {
  // SAFETY: This syscall has no side effects when passed NULL/0/CREATE_RULESET_VERSION.
  let rc = unsafe {
    libc::syscall(
      SYS_LANDLOCK_CREATE_RULESET,
      std::ptr::null::<LandlockRulesetAttr>(),
      0usize,
      LANDLOCK_CREATE_RULESET_VERSION,
    )
  };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(rc as u32)
}

fn landlock_create_ruleset(attr: &LandlockRulesetAttr) -> io::Result<OwnedFd> {
  // SAFETY: `attr` points to a properly-initialized `LandlockRulesetAttr`.
  let rc = unsafe {
    libc::syscall(
      SYS_LANDLOCK_CREATE_RULESET,
      attr as *const LandlockRulesetAttr,
      std::mem::size_of::<LandlockRulesetAttr>(),
      0u32,
    )
  };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  let fd: i32 = rc as i32;
  // SAFETY: `fd` is owned by us on successful syscall return.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn landlock_add_rule(
  ruleset_fd: i32,
  rule_type: u32,
  rule_attr: *const LandlockPathBeneathAttr,
) -> io::Result<()> {
  // SAFETY: `rule_attr` points to a properly-initialized rule struct (or the syscall returns an
  // error). `ruleset_fd` is a valid Landlock ruleset FD.
  let rc = unsafe {
    libc::syscall(
      SYS_LANDLOCK_ADD_RULE,
      ruleset_fd,
      rule_type,
      rule_attr,
      0u32,
    )
  };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn landlock_restrict_self(ruleset_fd: i32) -> io::Result<()> {
  // SAFETY: `ruleset_fd` is a valid Landlock ruleset FD.
  let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn add_path_rule(
  ruleset_fd: i32,
  path: &Path,
  access: LandlockAccess,
  handled_access_fs: u64,
) -> Result<(), LandlockError> {
  let allowed_access = access.bits() & handled_access_fs;
  if allowed_access == 0 {
    return Ok(());
  }

  let parent_fd = open_path_fd(path).map_err(|source| LandlockError::OpenPathFailed {
    path: path.to_path_buf(),
    source,
  })?;
  let attr = LandlockPathBeneathAttr {
    allowed_access,
    parent_fd: parent_fd.as_raw_fd(),
    _reserved: 0,
  };
  landlock_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &attr).map_err(|source| {
    LandlockError::AddRuleFailed {
      path: path.to_path_buf(),
      source,
    }
  })?;
  Ok(())
}

fn open_path_fd(path: &Path) -> io::Result<OwnedFd> {
  let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "path contains an embedded NUL byte",
    )
  })?;
  // SAFETY: `c_path` is a NUL-terminated string.
  let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: `fd` is owned by us and will be closed by `OwnedFd`.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
