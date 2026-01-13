use std::time::Duration;

/// Return the total CPU time (user + system) consumed by the current process so far.
///
/// This is intended for lightweight performance instrumentation (e.g. detecting idle CPU burn in
/// the windowed browser UI). The API is best-effort and returns `None` when unsupported or when the
/// underlying platform call fails.
///
/// - Unix (Linux/macOS): implemented via `libc::getrusage(RUSAGE_SELF, ...)`.
/// - Other targets: returns `None`.
pub fn current_process_cpu_time() -> Option<Duration> {
  current_process_cpu_time_impl()
}

#[cfg(unix)]
fn duration_from_timeval(tv: libc::timeval) -> Option<Duration> {
  // `timeval` uses signed types (`time_t`, `suseconds_t`). Be defensive and treat negative values
  // as invalid rather than panicking or underflowing.
  if tv.tv_sec < 0 || tv.tv_usec < 0 {
    return None;
  }

  let secs = u64::try_from(tv.tv_sec).ok()?;
  let usecs = u64::try_from(tv.tv_usec).ok()?;

  // `tv_usec` is *typically* in the range 0..1_000_000, but clamp/normalize in case a platform
  // provides a larger value.
  let extra_secs = usecs / 1_000_000;
  let rem_usecs = usecs % 1_000_000;

  let total_secs = secs.checked_add(extra_secs)?;
  let nanos = u32::try_from(rem_usecs.checked_mul(1000)?).ok()?;
  Some(Duration::new(total_secs, nanos))
}

#[cfg(unix)]
fn current_process_cpu_time_impl() -> Option<Duration> {
  use std::mem::MaybeUninit;

  let mut usage = MaybeUninit::<libc::rusage>::zeroed();
  // SAFETY: `getrusage` writes a full `rusage` struct on success.
  let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
  if rc != 0 {
    return None;
  }
  // SAFETY: `getrusage` succeeded, so `usage` is initialised.
  let usage = unsafe { usage.assume_init() };

  let user = duration_from_timeval(usage.ru_utime)?;
  let system = duration_from_timeval(usage.ru_stime)?;
  user.checked_add(system)
}

#[cfg(not(unix))]
fn current_process_cpu_time_impl() -> Option<Duration> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(unix)]
  #[test]
  fn timeval_to_duration_converts_microseconds() {
    let tv = libc::timeval {
      tv_sec: 1,
      tv_usec: 500_000,
    };
    assert_eq!(duration_from_timeval(tv), Some(Duration::from_micros(1_500_000)));
  }

  #[cfg(unix)]
  #[test]
  fn timeval_to_duration_normalizes_overflowing_usec() {
    let tv = libc::timeval {
      tv_sec: 1,
      tv_usec: 1_500_000,
    };
    assert_eq!(duration_from_timeval(tv), Some(Duration::from_micros(2_500_000)));
  }

  #[cfg(unix)]
  #[test]
  fn timeval_to_duration_rejects_negative_values() {
    let tv = libc::timeval {
      tv_sec: -1,
      tv_usec: 0,
    };
    assert_eq!(duration_from_timeval(tv), None);
  }

  #[cfg(unix)]
  #[test]
  fn current_process_cpu_time_is_available_on_unix() {
    assert!(
      current_process_cpu_time().is_some(),
      "expected getrusage-based CPU time to be available on unix targets"
    );
  }

  #[cfg(not(unix))]
  #[test]
  fn current_process_cpu_time_is_none_on_unsupported_targets() {
    assert!(
      current_process_cpu_time().is_none(),
      "expected current_process_cpu_time to be None on unsupported targets"
    );
  }
}

