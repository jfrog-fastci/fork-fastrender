//! In-process resource limits for FastRender CLIs.
//!
//! This module is intentionally small and reusable across binaries.
//! The primary guardrail is `RLIMIT_AS` (virtual address space), which provides a hard ceiling
//! that prevents runaway allocations from OOMing the host.


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpaceLimitStatus {
  /// Limit was disabled (`0` / unset).
  Disabled,
  /// Limit was applied successfully on a supported platform.
  Applied,
  /// The current platform does not support applying the limit.
  Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NofileLimitStatus {
  /// Limit was disabled (`0` / unset).
  Disabled,
  /// Limit was applied successfully on a supported platform.
  Applied,
  /// The current platform does not support applying the limit.
  Unsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum AddressSpaceLimitError {
  #[error("mem limit is too large: {limit_mb} MiB does not fit in platform rlimit type")]
  InvalidLimit { limit_mb: u64 },
  #[cfg(target_os = "linux")]
  #[error("failed to query RLIMIT_AS")]
  GetRlimitFailed {
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("failed to set RLIMIT_AS to {effective_bytes} bytes (requested {requested_mb} MiB)")]
  SetRlimitFailed {
    requested_mb: u64,
    effective_bytes: u64,
    #[source]
    source: io::Error,
  },
}

#[derive(Debug, thiserror::Error)]
pub enum NofileLimitError {
  #[error("nofile limit is too large: {limit} does not fit in platform rlimit type")]
  InvalidLimit { limit: u64 },
  #[cfg(target_os = "linux")]
  #[error("failed to query RLIMIT_NOFILE")]
  GetRlimitFailed {
    #[source]
    source: io::Error,
  },
  #[cfg(target_os = "linux")]
  #[error("failed to set RLIMIT_NOFILE to {effective} (requested {requested})")]
  SetRlimitFailed {
    requested: u64,
    effective: u64,
    #[source]
    source: io::Error,
  },
}

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Apply a hard address-space ceiling (`RLIMIT_AS`) for the current process.
///
/// - `limit_mb == 0` disables the guardrail.
/// - On Linux, this attempts to set `RLIMIT_AS` as early as possible during process startup.
/// - On non-Linux platforms, the call is a no-op and returns [`AddressSpaceLimitStatus::Unsupported`].
pub fn apply_address_space_limit_mb(
  limit_mb: u64,
) -> Result<AddressSpaceLimitStatus, AddressSpaceLimitError> {
  if limit_mb == 0 {
    return Ok(AddressSpaceLimitStatus::Disabled);
  }

  #[cfg(target_os = "linux")]
  {
    apply_address_space_limit_mb_linux(limit_mb)?;
    return Ok(AddressSpaceLimitStatus::Applied);
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = limit_mb;
    return Ok(AddressSpaceLimitStatus::Unsupported);
  }
}

/// Apply a file descriptor ceiling (`RLIMIT_NOFILE`) for the current process.
///
/// - `limit == 0` disables the guardrail.
/// - On Linux, this sets both the soft and hard ceiling to the minimum of the requested value and
///   the inherited hard maximum. This ensures we never attempt to raise the limit above what the
///   OS / sandbox has already granted.
/// - On non-Linux platforms, the call is a no-op and returns [`NofileLimitStatus::Unsupported`].
pub fn apply_nofile_limit(limit: u64) -> Result<NofileLimitStatus, NofileLimitError> {
  if limit == 0 {
    return Ok(NofileLimitStatus::Disabled);
  }

  #[cfg(target_os = "linux")]
  {
    apply_nofile_limit_linux(limit)?;
    return Ok(NofileLimitStatus::Applied);
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = limit;
    return Ok(NofileLimitStatus::Unsupported);
  }
}

#[cfg(target_os = "linux")]
fn apply_address_space_limit_mb_linux(limit_mb: u64) -> Result<(), AddressSpaceLimitError> {
  let requested_bytes = limit_mb
    .checked_mul(BYTES_PER_MIB)
    .ok_or(AddressSpaceLimitError::InvalidLimit { limit_mb })?;

  let requested_rlim: libc::rlim_t = requested_bytes
    .try_into()
    .map_err(|_| AddressSpaceLimitError::InvalidLimit { limit_mb })?;

  let (_cur, max) = get_address_space_limit_raw()
    .map_err(|source| AddressSpaceLimitError::GetRlimitFailed { source })?;

  // If the process is already constrained by an OS-level maximum (e.g. prlimit/cgroups),
  // enforce the minimum of the requested ceiling and the inherited hard max. This ensures we
  // never attempt to raise limits (which can fail under sandboxing) while still providing a
  // deterministic hard cap for the renderer.
  let effective = std::cmp::min(requested_rlim, max);
  let effective_bytes = effective as u64;

  let new = libc::rlimit {
    rlim_cur: effective,
    rlim_max: effective,
  };

  // SAFETY: `setrlimit` is a process-global syscall. We pass a properly-initialized `rlimit`.
  let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &new) };
  if rc != 0 {
    return Err(AddressSpaceLimitError::SetRlimitFailed {
      requested_mb: limit_mb,
      effective_bytes,
      source: io::Error::last_os_error(),
    });
  }
  Ok(())
}

#[cfg(target_os = "linux")]
fn apply_nofile_limit_linux(limit: u64) -> Result<(), NofileLimitError> {
  let requested_rlim: libc::rlim_t =
    limit.try_into().map_err(|_| NofileLimitError::InvalidLimit { limit })?;

  let (_cur, max) =
    get_nofile_limit_raw().map_err(|source| NofileLimitError::GetRlimitFailed { source })?;

  // Clamp to the inherited hard max to ensure we never attempt to raise limits (which can fail
  // under sandboxing) while still applying a deterministic ceiling for the process.
  let effective = std::cmp::min(requested_rlim, max);
  let effective_u64 = effective as u64;

  let new = libc::rlimit {
    rlim_cur: effective,
    rlim_max: effective,
  };

  // SAFETY: `setrlimit` is a process-global syscall. We pass a properly-initialized `rlimit`.
  let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) };
  if rc != 0 {
    return Err(NofileLimitError::SetRlimitFailed {
      requested: limit,
      effective: effective_u64,
      source: io::Error::last_os_error(),
    });
  }
  Ok(())
}

#[cfg(target_os = "linux")]
fn get_address_space_limit_raw() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
  let mut current = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `getrlimit` writes to `current` when the pointer is valid.
  let rc = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut current) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((current.rlim_cur, current.rlim_max))
}

#[cfg(target_os = "linux")]
fn get_nofile_limit_raw() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
  let mut current = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `getrlimit` writes to `current` when the pointer is valid.
  let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((current.rlim_cur, current.rlim_max))
}

/// Returns the current address-space limits (soft, hard) in bytes on Linux.
#[cfg(target_os = "linux")]
pub fn get_address_space_limit_bytes() -> io::Result<(u64, u64)> {
  let (cur, max) = get_address_space_limit_raw()?;
  Ok((cur as u64, max as u64))
}

/// Returns the current file descriptor limits (soft, hard) on Linux.
#[cfg(target_os = "linux")]
pub fn get_nofile_limit() -> io::Result<(u64, u64)> {
  let (cur, max) = get_nofile_limit_raw()?;
  Ok((cur as u64, max as u64))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::process::Command;

  #[cfg(target_os = "linux")]
  #[test]
  fn apply_address_space_limit_sets_rlimit_as() {
    const CHILD_ENV: &str = "FASTR_TEST_PROCESS_LIMITS_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let (_cur, max_bytes) =
        get_address_space_limit_bytes().expect("read rlimit in child process");
      let max_mb = max_bytes / BYTES_PER_MIB;
      assert!(max_mb > 0, "expected RLIMIT_AS max to be non-zero");
      let desired_mb = max_mb.min(8192).max(1);
      let status =
        apply_address_space_limit_mb(desired_mb).expect("apply address-space limit in child");
      assert_eq!(
        status,
        AddressSpaceLimitStatus::Applied,
        "expected limit to be applied"
      );
      let (cur, max) = get_address_space_limit_bytes().expect("read rlimit after applying limit");
      let expected = desired_mb * BYTES_PER_MIB;
      assert_eq!(
        cur, expected,
        "expected RLIMIT_AS.cur to match requested cap"
      );
      assert_eq!(
        max, expected,
        "expected RLIMIT_AS.max to match requested cap"
      );
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "process_limits::tests::apply_address_space_limit_sets_rlimit_as";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
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

  #[cfg(target_os = "linux")]
  #[test]
  fn apply_nofile_limit_sets_rlimit_nofile() {
    const CHILD_ENV: &str = "FASTR_TEST_PROCESS_LIMITS_NOFILE_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let (cur, _max) = get_nofile_limit().expect("read RLIMIT_NOFILE in child process");
      assert!(cur > 0, "expected RLIMIT_NOFILE.cur to be non-zero");
      let mut desired = cur.min(512);
      // Ensure the test actually lowers the ceiling when possible, so we exercise the setrlimit
      // code path (rather than re-applying an identical limit).
      if desired == cur && desired > 1 {
        desired -= 1;
      }
      let status = apply_nofile_limit(desired).expect("apply nofile limit in child");
      assert_eq!(
        status,
        NofileLimitStatus::Applied,
        "expected limit to be applied"
      );
      let (cur_after, max_after) =
        get_nofile_limit().expect("read RLIMIT_NOFILE after applying limit");
      assert_eq!(
        cur_after, desired,
        "expected RLIMIT_NOFILE.cur to match requested cap"
      );
      assert_eq!(
        max_after, desired,
        "expected RLIMIT_NOFILE.max to match requested cap"
      );
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "process_limits::tests::apply_nofile_limit_sets_rlimit_nofile";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
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
