use crate::ipc::protocol::cancel::CancelGensSnapshot;
use crate::render_control::CancelCallback;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Renderer-side cooperative cancellation generations.
///
/// Unlike `ui::cancel::CancelGens`, this type is not shared with the browser via `Arc` across
/// threads. Instead, the browser sends [`CancelGensSnapshot`] updates over IPC and the renderer
/// stores the latest values in atomics.
#[derive(Debug, Default)]
pub struct CancelGens {
  nav: AtomicU64,
  paint: AtomicU64,
}

impl CancelGens {
  pub fn new() -> Self {
    Self::default()
  }

  /// Apply an updated cancellation generation snapshot received over IPC.
  ///
  /// This is monotonic: generations never decrease, which prevents "un-cancelling" in-flight work
  /// if messages are duplicated or reordered.
  pub fn apply_snapshot(&self, snapshot: CancelGensSnapshot) {
    // Update paint first to preserve the common invariant `paint >= nav` when the sender uses the
    // same bumping contract as the in-process cancellation gens.
    self.paint.fetch_max(snapshot.paint, Ordering::Relaxed);
    self.nav.fetch_max(snapshot.nav, Ordering::Relaxed);
  }

  /// Create a snapshot for prepare/layout work.
  ///
  /// Prepare stages ignore paint bumps: repaint requests should not cancel an in-flight navigation
  /// that might still commit.
  pub fn snapshot_prepare(&self) -> CancelGensSnapshot {
    CancelGensSnapshot {
      nav: self.nav.load(Ordering::Relaxed),
      paint: 0,
    }
  }

  /// Create a snapshot for paint work.
  ///
  /// Paint stages cancel on any bump (nav or paint).
  pub fn snapshot_paint(&self) -> CancelGensSnapshot {
    CancelGensSnapshot {
      nav: self.nav.load(Ordering::Relaxed),
      paint: self.paint.load(Ordering::Relaxed),
    }
  }
}

impl CancelGensSnapshot {
  pub fn cancel_callback_for_prepare(&self, gens: &Arc<CancelGens>) -> Arc<CancelCallback> {
    let expected_nav = self.nav;
    let gens = Arc::clone(gens);
    Arc::new(move || gens.nav.load(Ordering::Relaxed) != expected_nav)
  }

  pub fn cancel_callback_for_paint(&self, gens: &Arc<CancelGens>) -> Arc<CancelCallback> {
    let expected_nav = self.nav;
    let expected_paint = self.paint;
    let gens = Arc::clone(gens);
    Arc::new(move || {
      gens.nav.load(Ordering::Relaxed) != expected_nav
        || gens.paint.load(Ordering::Relaxed) != expected_paint
    })
  }

  pub fn is_still_current_for_prepare(&self, gens: &CancelGens) -> bool {
    gens.nav.load(Ordering::Relaxed) == self.nav
  }

  pub fn is_still_current_for_paint(&self, gens: &CancelGens) -> bool {
    gens.nav.load(Ordering::Relaxed) == self.nav && gens.paint.load(Ordering::Relaxed) == self.paint
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn prepare_callback_ignores_paint_bumps() {
    let gens = Arc::new(CancelGens::new());
    let snapshot = gens.snapshot_prepare();
    let callback = snapshot.cancel_callback_for_prepare(&gens);

    assert!(!callback());
    assert!(snapshot.is_still_current_for_prepare(&gens));

    // Paint-only bump should not cancel prepare work.
    gens.apply_snapshot(CancelGensSnapshot { nav: 0, paint: 1 });
    assert!(!callback());
    assert!(snapshot.is_still_current_for_prepare(&gens));

    // Nav bump cancels prepare.
    gens.apply_snapshot(CancelGensSnapshot { nav: 1, paint: 2 });
    assert!(callback());
    assert!(!snapshot.is_still_current_for_prepare(&gens));
  }

  #[test]
  fn paint_callback_cancels_on_any_bump() {
    let gens = Arc::new(CancelGens::new());
    let snapshot = gens.snapshot_paint();
    let callback = snapshot.cancel_callback_for_paint(&gens);

    assert!(!callback());
    assert!(snapshot.is_still_current_for_paint(&gens));

    gens.apply_snapshot(CancelGensSnapshot { nav: 0, paint: 1 });
    assert!(callback());
    assert!(!snapshot.is_still_current_for_paint(&gens));

    let snapshot = gens.snapshot_paint();
    let callback = snapshot.cancel_callback_for_paint(&gens);
    assert!(!callback());
    assert!(snapshot.is_still_current_for_paint(&gens));

    gens.apply_snapshot(CancelGensSnapshot { nav: 1, paint: 2 });
    assert!(callback());
    assert!(!snapshot.is_still_current_for_paint(&gens));
  }

  #[test]
  fn snapshots_are_stable() {
    let gens = CancelGens::new();

    let prepare_a = gens.snapshot_prepare();
    gens.apply_snapshot(CancelGensSnapshot { nav: 0, paint: 1 });
    let prepare_b = gens.snapshot_prepare();
    assert_eq!(prepare_a, prepare_b, "prepare snapshots ignore paint bumps");

    let paint_a = gens.snapshot_paint();
    let paint_b = gens.snapshot_paint();
    assert_eq!(paint_a, paint_b);

    gens.apply_snapshot(CancelGensSnapshot { nav: 0, paint: 2 });
    let paint_c = gens.snapshot_paint();
    assert_ne!(paint_a, paint_c);
  }

  #[test]
  fn apply_snapshot_is_monotonic() {
    let gens = CancelGens::new();

    gens.apply_snapshot(CancelGensSnapshot { nav: 5, paint: 6 });
    // Older updates should not regress (e.g. if messages are duplicated/reordered).
    gens.apply_snapshot(CancelGensSnapshot { nav: 4, paint: 1 });

    assert_eq!(gens.snapshot_paint(), CancelGensSnapshot { nav: 5, paint: 6 });
  }
}

