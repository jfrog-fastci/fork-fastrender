use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Coalesces "wake" notifications so at most one is outstanding at any time.
///
/// This is used by the windowed browser UI to avoid flooding the winit event loop with one user
/// event per worker message (e.g. during scrolling). The UI thread clears the pending flag when it
/// handles the wake event, allowing the worker bridge thread to schedule a new wake if more work
/// arrives.
#[derive(Debug, Clone)]
pub struct WorkerWakeCoalescer {
  pending: Arc<AtomicBool>,
}

impl WorkerWakeCoalescer {
  pub fn new(pending: Arc<AtomicBool>) -> Self {
    Self { pending }
  }

  /// Request a wakeup by invoking `send` at most once until [`Self::clear_pending`] is called.
  ///
  /// Returns `true` if `send` was invoked (i.e. this call transitioned the wake state from
  /// "not pending" → "pending"), otherwise returns `false` when a wake is already pending.
  pub fn request_wake<E>(&self, send: impl FnOnce() -> Result<(), E>) -> bool {
    if self.pending.fetch_or(true, Ordering::AcqRel) {
      return false;
    }
    if send().is_err() {
      // If we failed to enqueue a wake event (e.g. the winit event loop already shut down),
      // clear the flag so subsequent calls can retry.
      self.pending.store(false, Ordering::Release);
    }
    true
  }

  /// Mark any outstanding wake request as handled.
  pub fn clear_pending(&self) {
    self.pending.store(false, Ordering::Release);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::AtomicUsize;

  #[test]
  fn request_wake_only_sends_once_until_cleared() {
    let pending = Arc::new(AtomicBool::new(false));
    let coalescer = WorkerWakeCoalescer::new(Arc::clone(&pending));

    let sends = AtomicUsize::new(0);
    coalescer.request_wake(|| {
      sends.fetch_add(1, Ordering::SeqCst);
      Ok::<(), ()>(())
    });
    coalescer.request_wake(|| {
      sends.fetch_add(1, Ordering::SeqCst);
      Ok::<(), ()>(())
    });
    coalescer.request_wake(|| {
      sends.fetch_add(1, Ordering::SeqCst);
      Ok::<(), ()>(())
    });
    assert_eq!(sends.load(Ordering::SeqCst), 1);

    coalescer.clear_pending();
    coalescer.request_wake(|| {
      sends.fetch_add(1, Ordering::SeqCst);
      Ok::<(), ()>(())
    });
    assert_eq!(sends.load(Ordering::SeqCst), 2);
  }
}
