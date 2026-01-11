use std::sync::Arc;

use crate::sync::GcAwareMutex;

/// Errors produced while attempting to pin buffers for I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IoLimitError {
  #[error("limit exceeded: {0}")]
  LimitExceeded(&'static str),
  #[error("invalid pin range")]
  InvalidRange,
  #[error("buffer is detached or not alive")]
  BufferNotAlive,
  #[error("buffer is in use by another in-flight I/O operation")]
  BufferBorrowed,
}

impl From<IoLimitError> for std::io::Error {
  fn from(value: IoLimitError) -> Self {
    std::io::Error::new(std::io::ErrorKind::Other, value)
  }
}

/// Hard limits for pinned external buffers and in-flight I/O operations.
///
/// These limits provide DoS resistance against hostile code that starts many async operations
/// (or stalls them) while pinning large external buffers that cannot be reclaimed until
/// completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoLimits {
  /// Maximum total number of bytes that may be pinned at once across all in-flight I/O ops.
  pub max_pinned_bytes: usize,
  /// Maximum number of in-flight I/O operations.
  pub max_inflight_ops: usize,
  /// Optional per-op cap to prevent a single op from pinning an arbitrarily large buffer.
  pub max_pinned_bytes_per_op: Option<usize>,
}

impl Default for IoLimits {
  fn default() -> Self {
    // These defaults are intentionally conservative. Embeddings that need higher throughput can
    // override them.
    Self {
      // 64MiB across all in-flight operations.
      max_pinned_bytes: 64 * 1024 * 1024,
      // Plenty of ops, but still bounded.
      max_inflight_ops: 1024,
      // No per-op cap by default; the global cap is still enforced.
      max_pinned_bytes_per_op: None,
    }
  }
}

/// Snapshot of current I/O pinning counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoCounters {
  pub pinned_bytes_current: usize,
  pub inflight_ops_current: usize,
}

#[derive(Debug, Default)]
struct IoState {
  pinned_bytes_current: usize,
  inflight_ops_current: usize,
}

/// Shared I/O limiter and accounting state.
///
/// This type is expected to be owned by the embedding/runtime and shared across threads.
#[derive(Debug)]
pub struct IoLimiter {
  limits: IoLimits,
  state: GcAwareMutex<IoState>,
}

impl IoLimiter {
  pub fn new(limits: IoLimits) -> Self {
    Self {
      limits,
      state: GcAwareMutex::new(IoState::default()),
    }
  }

  #[inline]
  pub fn limits(&self) -> IoLimits {
    self.limits
  }

  /// Returns the current pinning counters.
  pub fn counters(&self) -> IoCounters {
    let state = self.state.lock();
    IoCounters {
      pinned_bytes_current: state.pinned_bytes_current,
      inflight_ops_current: state.inflight_ops_current,
    }
  }

  /// Attempts to reserve accounting capacity for a single in-flight I/O operation that pins
  /// `pinned_bytes` bytes.
  ///
  /// On success, returns an RAII permit that releases the counters when dropped (completion or
  /// cancellation).
  pub(crate) fn try_acquire(self: &Arc<Self>, pinned_bytes: usize) -> Result<IoPermit, IoLimitError> {
    if let Some(max) = self.limits.max_pinned_bytes_per_op {
      if pinned_bytes > max {
        return Err(IoLimitError::LimitExceeded("max pinned bytes per op"));
      }
    }

    let mut state = self.state.lock();

    let new_inflight = state
      .inflight_ops_current
      .checked_add(1)
      .ok_or(IoLimitError::LimitExceeded("max inflight ops"))?;
    if new_inflight > self.limits.max_inflight_ops {
      return Err(IoLimitError::LimitExceeded("max inflight ops"));
    }

    let new_pinned = state
      .pinned_bytes_current
      .checked_add(pinned_bytes)
      .ok_or(IoLimitError::LimitExceeded("max pinned bytes"))?;
    if new_pinned > self.limits.max_pinned_bytes {
      return Err(IoLimitError::LimitExceeded("max pinned bytes"));
    }

    state.inflight_ops_current = new_inflight;
    state.pinned_bytes_current = new_pinned;

    Ok(IoPermit {
      limiter: Arc::clone(self),
      pinned_bytes,
      inflight_ops: 1,
    })
  }
}

/// RAII permit that releases pinned buffer + in-flight op accounting on drop.
#[derive(Debug)]
pub(crate) struct IoPermit {
  limiter: Arc<IoLimiter>,
  pinned_bytes: usize,
  inflight_ops: usize,
}

impl Drop for IoPermit {
  fn drop(&mut self) {
    let mut state = self.limiter.state.lock();
    debug_assert!(state.pinned_bytes_current >= self.pinned_bytes);
    debug_assert!(state.inflight_ops_current >= self.inflight_ops);
    state.pinned_bytes_current = state.pinned_bytes_current.saturating_sub(self.pinned_bytes);
    state.inflight_ops_current = state.inflight_ops_current.saturating_sub(self.inflight_ops);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn io_limiter_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    // Stop-the-world handshakes can take much longer in debug builds (especially
    // under parallel test execution on multi-agent hosts). Keep release builds
    // strict, but give debug builds enough slack to avoid flaky timeouts.
    const TIMEOUT: Duration = if cfg!(debug_assertions) {
      Duration::from_secs(30)
    } else {
      Duration::from_secs(2)
    };
    let limiter = Arc::new(IoLimiter::new(IoLimits::default()));

    std::thread::scope(|scope| {
      // Thread A holds the limiter state lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to read counters while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<IoCounters>();

      let limiter_a = Arc::clone(&limiter);
      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = limiter_a.state.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the limiter lock");

      let limiter_c = Arc::clone(&limiter);
      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();
        c_start_rx.recv().unwrap();

        let counters = limiter_c.counters();
        c_done_tx.send(counters).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the limiter lock before starting STW.
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
      let start = Instant::now();
      loop {
        let mut native_safe = false;
        threading::registry::for_each_thread(|t| {
          if t.id() == c_id {
            native_safe = t.is_native_safe();
          }
        });

        if native_safe {
          break;
        }
        if start.elapsed() > TIMEOUT {
          panic!("thread C did not enter a GC-safe region while blocked on the limiter lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      struct ResumeOnDrop;
      impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
          crate::threading::safepoint::rt_gc_resume_world();
        }
      }
      let _resume = ResumeOnDrop;

      // Let thread A release the lock and reach the safepoint.
      a_release_tx.send(()).unwrap();

      assert!(
        crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
        "world failed to stop within timeout; limiter lock contention must not block STW"
      );

      // Resume the world so the contending lock acquisition can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      let counters = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should finish after world is resumed");
      assert_eq!(
        counters,
        IoCounters {
          pinned_bytes_current: 0,
          inflight_ops_current: 0,
        }
      );
    });
  }
}
