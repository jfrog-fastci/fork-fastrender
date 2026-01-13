//! Backwards-compatible JS clock module.
//!
//! The shared monotonic clock abstraction lives in [`crate::clock`]. This module remains as a
//! re-export so existing `crate::js::clock::*` paths keep working.
//!
//! Deterministic hosts can advance virtual/test clocks via [`Clock::advance`]. This is best-effort:
//! it has an effect only for clocks that support deterministic stepping (e.g. [`VirtualClock`]);
//! real-time clocks ignore it.

pub use crate::clock::{Clock, RealClock, VirtualClock};

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  use std::time::Duration;

  #[test]
  fn clock_advance_updates_virtual_clock_through_trait_object() {
    let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new());
    assert_eq!(clock.now(), Duration::ZERO);

    clock.advance(Duration::from_millis(5));
    assert_eq!(clock.now(), Duration::from_millis(5));
  }

  #[test]
  fn clock_advance_is_noop_for_real_clock() {
    let clock: Arc<dyn Clock> = Arc::new(RealClock::default());
    let t0 = clock.now();

    // `RealClock` cannot be deterministically advanced; this should be a no-op and must not panic.
    clock.advance(Duration::from_secs(10));
    let t1 = clock.now();
    assert!(t1 >= t0);
  }
}
