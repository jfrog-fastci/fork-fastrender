use parking_lot::Mutex;
use std::sync::Arc;

/// Errors produced while attempting to pin buffers for I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IoLimitError {
  #[error("limit exceeded: {0}")]
  LimitExceeded(&'static str),
  #[error("invalid pin range")]
  InvalidRange,
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
  state: Mutex<IoState>,
}

impl IoLimiter {
  pub fn new(limits: IoLimits) -> Self {
    Self {
      limits,
      state: Mutex::new(IoState::default()),
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
