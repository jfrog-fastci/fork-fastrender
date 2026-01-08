use crate::render_control::{CancelCallback, RenderDeadline};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct CancelGens {
  nav: Arc<AtomicU64>,
  paint: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelSnapshot {
  nav: u64,
  paint: u64,
}

impl CancelGens {
  pub fn new() -> Self {
    Self {
      nav: Arc::new(AtomicU64::new(0)),
      paint: Arc::new(AtomicU64::new(0)),
    }
  }

  pub fn bump_nav(&self) {
    self.nav.fetch_add(1, Ordering::Relaxed);
    self.paint.fetch_add(1, Ordering::Relaxed);
  }

  pub fn bump_paint(&self) {
    self.paint.fetch_add(1, Ordering::Relaxed);
  }

  pub fn snapshot_prepare(&self) -> CancelSnapshot {
    CancelSnapshot {
      nav: self.nav.load(Ordering::Relaxed),
      paint: 0,
    }
  }

  pub fn snapshot_paint(&self) -> CancelSnapshot {
    CancelSnapshot {
      nav: self.nav.load(Ordering::Relaxed),
      paint: self.paint.load(Ordering::Relaxed),
    }
  }
}

impl Default for CancelGens {
  fn default() -> Self {
    Self::new()
  }
}

impl CancelSnapshot {
  pub fn cancel_callback_for_prepare(&self, gens: &CancelGens) -> Arc<CancelCallback> {
    let expected_nav = self.nav;
    let gens_nav = Arc::clone(&gens.nav);
    Arc::new(move || gens_nav.load(Ordering::Relaxed) != expected_nav)
  }

  pub fn cancel_callback_for_paint(&self, gens: &CancelGens) -> Arc<CancelCallback> {
    let expected_nav = self.nav;
    let expected_paint = self.paint;
    let gens_nav = Arc::clone(&gens.nav);
    let gens_paint = Arc::clone(&gens.paint);
    Arc::new(move || {
      gens_nav.load(Ordering::Relaxed) != expected_nav
        || gens_paint.load(Ordering::Relaxed) != expected_paint
    })
  }
}

pub fn deadline_for(callback: Arc<CancelCallback>, timeout: Option<Duration>) -> RenderDeadline {
  RenderDeadline::new(timeout, Some(callback))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn prepare_callback_ignores_paint_bumps() {
    let gens = CancelGens::new();
    let snapshot = gens.snapshot_prepare();
    let callback = snapshot.cancel_callback_for_prepare(&gens);

    assert!(!callback());
    gens.bump_paint();
    assert!(!callback());
    gens.bump_nav();
    assert!(callback());
  }

  #[test]
  fn paint_callback_cancels_on_any_bump() {
    let gens = CancelGens::new();
    let snapshot = gens.snapshot_paint();
    let callback = snapshot.cancel_callback_for_paint(&gens);

    assert!(!callback());
    gens.bump_paint();
    assert!(callback());

    let snapshot = gens.snapshot_paint();
    let callback = snapshot.cancel_callback_for_paint(&gens);
    assert!(!callback());
    gens.bump_nav();
    assert!(callback());
  }

  #[test]
  fn snapshots_are_stable() {
    let gens = CancelGens::new();

    let prepare_a = gens.snapshot_prepare();
    gens.bump_paint();
    let prepare_b = gens.snapshot_prepare();
    assert_eq!(prepare_a, prepare_b, "prepare snapshots ignore paint bumps");

    let paint_a = gens.snapshot_paint();
    let paint_b = gens.snapshot_paint();
    assert_eq!(paint_a, paint_b);

    gens.bump_paint();
    let paint_c = gens.snapshot_paint();
    assert_ne!(paint_a, paint_c);

    let cb: Arc<CancelCallback> = Arc::new(|| false);
    let timeout = Some(Duration::from_millis(50));
    let deadline = deadline_for(cb, timeout);
    assert_eq!(deadline.timeout_limit(), timeout);
    assert!(deadline.cancel_callback().is_some());
  }
}
