use super::registry;
use super::set_parked;

/// RAII guard that marks the current thread as `parked` for the duration of a
/// potentially-blocking runtime operation (locks, syscalls, ...).
///
/// While parked, the stop-the-world coordinator treats the thread as already
/// quiescent and will not wait for it to observe the current safepoint epoch.
pub struct ParkedGuard {
  was_parked: bool,
}

impl ParkedGuard {
  #[inline]
  pub fn new() -> Self {
    let was_parked = registry::current_thread_state().is_some_and(|s| s.is_parked());
    set_parked(true);
    Self { was_parked }
  }
}

impl Drop for ParkedGuard {
  #[inline]
  fn drop(&mut self) {
    // Preserve an outer/manual parking state (e.g. an already-idle worker).
    if !self.was_parked {
      set_parked(false);
    }
  }
}

/// Execute `f` while treating the current thread as parked.
#[inline]
pub fn park_while<F, R>(f: F) -> R
where
  F: FnOnce() -> R,
{
  let _guard = ParkedGuard::new();
  f()
}
