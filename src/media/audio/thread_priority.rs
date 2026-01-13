//! Best-effort thread priority tweaks for audio callbacks / engines.
//!
//! Audio render threads are latency-sensitive; if they lose CPU time for too long, the OS output
//! buffer underflows and playback glitches. This module tries to nudge the current thread into a
//! higher-priority scheduling class where supported by the platform.
//!
//! All operations are best-effort:
//! - Failures never panic.
//! - Failures only emit a single debug log per process.
//! - Unsupported platforms are no-ops.

use std::cell::Cell;
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};

/// Attempt to raise the priority of the *current thread* for audio work.
///
/// Intended to be called at the start of an audio callback thread / audio engine thread. Safe to
/// call multiple times; the operation is attempted at most once per thread.
pub(crate) fn promote_current_thread_for_audio() {
  thread_local! {
    static DID_PROMOTE: Cell<bool> = Cell::new(false);
  }

  DID_PROMOTE.with(|did_promote| {
    if did_promote.get() {
      return;
    }
    // Mark before doing any OS work so we don't retry (or re-log) even if the attempt fails.
    did_promote.set(true);
    if let Err(err) = platform_promote_current_thread_for_audio() {
      log_failure_once(err);
    }
  });
}

#[derive(Debug)]
enum PromoteError {
  #[cfg(target_os = "windows")]
  Mmcss(std::io::Error),
  #[cfg(target_os = "macos")]
  PthreadQos(std::io::Error),
}

fn platform_promote_current_thread_for_audio() -> Result<(), PromoteError> {
  #[cfg(target_os = "windows")]
  {
    windows::promote_current_thread_for_audio().map_err(PromoteError::Mmcss)?;
    return Ok(());
  }

  #[cfg(target_os = "macos")]
  {
    macos::promote_current_thread_for_audio().map_err(PromoteError::PthreadQos)?;
    return Ok(());
  }

  // Linux / other UNIXes: do not attempt real-time scheduling by default (may require privileges).
  Ok(())
}

fn log_failure_once(err: PromoteError) {
  // `eprintln!` is used for debug-only diagnostics throughout this codebase. Keep this best-effort
  // warning lightweight: if promotion fails in CI/unprivileged environments, that's expected.
  #[cfg(debug_assertions)]
  {
    static DID_LOG: AtomicBool = AtomicBool::new(false);
    if DID_LOG.swap(true, Ordering::Relaxed) {
      return;
    }

    match err {
      #[cfg(target_os = "windows")]
      PromoteError::Mmcss(err) => {
        eprintln!("audio thread priority: failed to enable MMCSS for current thread: {err}");
      }
      #[cfg(target_os = "macos")]
      PromoteError::PthreadQos(err) => {
        eprintln!("audio thread priority: failed to set QoS class for current thread: {err}");
      }
    }
  }

  #[cfg(not(debug_assertions))]
  {
    let _ = err;
  }
}

#[cfg(target_os = "windows")]
mod windows {
  use std::cell::RefCell;
  #[cfg(debug_assertions)]
  use std::sync::atomic::{AtomicBool, Ordering};

  use core::ffi::c_void;

  const MMCSS_TASK_PRO_AUDIO: &[u16] = &[
    'P' as u16, 'r' as u16, 'o' as u16, ' ' as u16, 'A' as u16, 'u' as u16, 'd' as u16, 'i' as u16,
    'o' as u16, 0,
  ];

  thread_local! {
    // Keep the MMCSS registration handle alive for the lifetime of the thread so it can be
    // reverted when the thread exits.
    static MMCSS_GUARD: RefCell<Option<MmcssGuard>> = RefCell::new(None);
  }

  pub(super) fn promote_current_thread_for_audio() -> Result<(), std::io::Error> {
    MMCSS_GUARD.with(|guard| {
      if guard.borrow().is_some() {
        return Ok(());
      }

      let mmcss = MmcssGuard::new()?;
      *guard.borrow_mut() = Some(mmcss);
      Ok(())
    })
  }

  struct MmcssGuard {
    handle: *mut c_void,
  }

  impl MmcssGuard {
    fn new() -> Result<Self, std::io::Error> {
      let mut task_index: u32 = 0;
      // SAFETY: FFI call; `MMCSS_TASK_PRO_AUDIO` is a null-terminated UTF-16 string.
      let handle =
        unsafe { AvSetMmThreadCharacteristicsW(MMCSS_TASK_PRO_AUDIO.as_ptr(), &mut task_index) };
      if handle.is_null() {
        return Err(std::io::Error::last_os_error());
      }
      Ok(Self { handle })
    }
  }

  impl Drop for MmcssGuard {
    fn drop(&mut self) {
      // SAFETY: `self.handle` is a handle returned from `AvSetMmThreadCharacteristicsW`.
      let ok = unsafe { AvRevertMmThreadCharacteristics(self.handle) };
      if ok == 0 {
        // Revert failures are not actionable; ignore (but keep a best-effort single debug log).
        #[cfg(debug_assertions)]
        {
          static DID_LOG: AtomicBool = AtomicBool::new(false);
          if !DID_LOG.swap(true, Ordering::Relaxed) {
            eprintln!(
              "audio thread priority: failed to revert MMCSS thread characteristics: {}",
              std::io::Error::last_os_error()
            );
          }
        }
      }
    }
  }

  #[link(name = "avrt")]
  extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
  }
}

#[cfg(target_os = "macos")]
mod macos {
  // From `<pthread/qos.h>`.
  // See: https://developer.apple.com/documentation/apple-silicon/thread_qos
  const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

  pub(super) fn promote_current_thread_for_audio() -> Result<(), std::io::Error> {
    // SAFETY: FFI call; sets QoS for the current thread.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc == 0 {
      Ok(())
    } else {
      Err(std::io::Error::from_raw_os_error(rc))
    }
  }

  extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn promote_current_thread_for_audio_is_best_effort() {
    promote_current_thread_for_audio();
    // Re-entrant calls should be no-ops.
    promote_current_thread_for_audio();
  }
}
