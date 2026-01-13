//! Scroll-position session autosave throttling for crash recovery.
//!
//! The browser session snapshot stores per-tab `scroll_css` offsets so that a crash/unclean exit
//! restore can bring the user back near where they were. Updating the on-disk session on every
//! scroll event would be expensive (building a full snapshot + IPC to the autosave worker), so the
//! windowed UI uses a small throttle:
//! - while the user is scrolling, request at most one autosave per interval (e.g. 2s)
//! - when scrolling stops, request one final autosave after a short idle debounce (e.g. 750ms)
//! - ignore tiny scroll changes below a threshold (e.g. 128 CSS px)
//!
//! This module is intentionally UI-backend agnostic and depends only on lightweight UI types
//! (`TabId`), so it can be unit-tested without winit/egui.

use crate::ui::TabId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default maximum frequency for scroll-driven session autosaves while the user is scrolling.
pub const DEFAULT_SCROLL_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);
/// Default idle debounce used to trigger a final autosave after scrolling stops.
pub const DEFAULT_SCROLL_AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(750);
/// Default minimum scroll delta (CSS px) required before we request a scroll-driven autosave.
pub const DEFAULT_SCROLL_AUTOSAVE_THRESHOLD_CSS: f32 = 128.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollAutosaveConfig {
  /// Minimum interval between autosave requests while scrolling.
  pub interval: Duration,
  /// Idle debounce after the last observed scroll change.
  pub debounce: Duration,
  /// Minimum scroll delta (in CSS px) before scroll-only autosave triggers.
  pub threshold_css: f32,
}

impl Default for ScrollAutosaveConfig {
  fn default() -> Self {
    Self {
      interval: DEFAULT_SCROLL_AUTOSAVE_INTERVAL,
      debounce: DEFAULT_SCROLL_AUTOSAVE_DEBOUNCE,
      threshold_css: DEFAULT_SCROLL_AUTOSAVE_THRESHOLD_CSS,
    }
  }
}

/// Per-window scroll autosave throttle state.
#[derive(Debug)]
pub struct ScrollAutosaveThrottle {
  cfg: ScrollAutosaveConfig,
  /// True when at least one viewport scroll change has been observed since the last autosave (or
  /// since the start of the current scroll burst).
  dirty: bool,
  /// When the current "scroll burst" began (first observed scroll change since the last autosave).
  dirty_started_at: Option<Instant>,
  /// When the most recent scroll change was observed.
  last_change_at: Option<Instant>,
  /// When the last scroll-driven autosave was requested.
  last_autosave_at: Option<Instant>,
  /// Baseline scroll offsets from the last autosaved snapshot (or session restore), keyed by tab.
  baseline_scroll_css: HashMap<TabId, (f32, f32)>,
  /// Next desired wakeup deadline for servicing the throttle (debounce/interval).
  next_deadline: Option<Instant>,
}

impl Default for ScrollAutosaveThrottle {
  fn default() -> Self {
    Self::new(ScrollAutosaveConfig::default())
  }
}

impl ScrollAutosaveThrottle {
  pub fn new(cfg: ScrollAutosaveConfig) -> Self {
    Self {
      cfg,
      dirty: false,
      dirty_started_at: None,
      last_change_at: None,
      last_autosave_at: None,
      baseline_scroll_css: HashMap::new(),
      next_deadline: None,
    }
  }

  /// Record the persisted/session baseline scroll offset for a tab.
  ///
  /// Callers restoring a session should seed this so the initial scroll restore does not
  /// immediately trigger a redundant autosave.
  pub fn set_tab_baseline_scroll_css(&mut self, tab_id: TabId, scroll_css: (f32, f32)) {
    self.baseline_scroll_css.insert(tab_id, sanitize_scroll_css(scroll_css));
  }

  /// Mark that a scroll change occurred.
  pub fn observe_scroll_change(&mut self, now: Instant) {
    if !self.dirty {
      self.dirty = true;
      self.dirty_started_at = Some(now);
    }
    self.last_change_at = Some(now);
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.next_deadline
  }

  /// Advance the throttle state and decide whether the caller should request a session autosave
  /// now.
  ///
  /// `scrolls` is supplied as a closure so we can iterate the current scroll positions multiple
  /// times without allocating.
  pub fn tick<F, I>(&mut self, now: Instant, mut scrolls: F) -> bool
  where
    F: FnMut() -> I,
    I: IntoIterator<Item = (TabId, (f32, f32))>,
  {
    if !self.dirty {
      self.next_deadline = None;
      return false;
    }

    let Some(last_change_at) = self.last_change_at else {
      // Be defensive: treat missing timestamps as "not dirty".
      self.dirty = false;
      self.dirty_started_at = None;
      self.next_deadline = None;
      return false;
    };

    let max_delta = self.max_scroll_delta_css(scrolls());
    let significant = max_delta >= self.cfg.threshold_css;

    let debounce_deadline = last_change_at.checked_add(self.cfg.debounce);

    if !significant {
      // Small scroll changes are ignored; clear them once the user has been idle for the debounce
      // window so we stop scheduling wakeups.
      if debounce_deadline.is_some_and(|d| now >= d) {
        self.dirty = false;
        self.dirty_started_at = None;
        self.last_change_at = None;
        self.next_deadline = None;
      } else {
        self.next_deadline = debounce_deadline;
      }
      return false;
    }

    let interval_deadline = if let Some(last) = self.last_autosave_at {
      last.checked_add(self.cfg.interval)
    } else {
      self
        .dirty_started_at
        .and_then(|start| start.checked_add(self.cfg.interval))
    };

    let due_interval = interval_deadline.is_some_and(|d| now >= d);
    let due_idle = debounce_deadline.is_some_and(|d| now >= d);

    if due_interval || due_idle {
      // Request autosave now and reset burst state.
      self.last_autosave_at = Some(now);
      self.dirty = false;
      self.dirty_started_at = None;
      self.last_change_at = None;
      self.next_deadline = None;

      // Update baselines to the snapshot we're about to persist.
      self.baseline_scroll_css.clear();
      for (tab_id, scroll_css) in scrolls() {
        self
          .baseline_scroll_css
          .insert(tab_id, sanitize_scroll_css(scroll_css));
      }
      return true;
    }

    self.next_deadline = earliest_deadline(interval_deadline, debounce_deadline);
    false
  }

  fn max_scroll_delta_css<I>(&self, scrolls: I) -> f32
  where
    I: IntoIterator<Item = (TabId, (f32, f32))>,
  {
    let mut max_delta = 0.0f32;
    for (tab_id, scroll_css) in scrolls {
      let current = sanitize_scroll_css(scroll_css);
      let base = self
        .baseline_scroll_css
        .get(&tab_id)
        .copied()
        .unwrap_or((0.0, 0.0));
      let dx = (current.0 - base.0).abs();
      let dy = (current.1 - base.1).abs();
      max_delta = max_delta.max(dx.max(dy));
    }
    max_delta
  }
}

fn sanitize_scroll_css(raw: (f32, f32)) -> (f32, f32) {
  let x = if raw.0.is_finite() { raw.0.max(0.0) } else { 0.0 };
  let y = if raw.1.is_finite() { raw.1.max(0.0) } else { 0.0 };
  (x, y)
}

fn earliest_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
  match (a, b) {
    (Some(a), Some(b)) => Some(a.min(b)),
    (Some(a), None) => Some(a),
    (None, Some(b)) => Some(b),
    (None, None) => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rapid_scroll_updates_coalesce_to_one_autosave_per_interval() {
    let cfg = ScrollAutosaveConfig {
      interval: Duration::from_secs(2),
      debounce: Duration::from_millis(750),
      threshold_css: 128.0,
    };
    let mut throttle = ScrollAutosaveThrottle::new(cfg);
    let tab = TabId(1);
    throttle.set_tab_baseline_scroll_css(tab, (0.0, 0.0));

    let start = Instant::now();
    let mut scroll = (0.0f32, 0.0f32);
    let mut saves = 0usize;

    // Simulate a continuous scroll stream for 3.5s (updates every 100ms). We should request at
    // most one autosave within the first 2s interval, and exactly one by 3.5s (at t=2s).
    for i in 0..=35u64 {
      let now = start + Duration::from_millis(i * 100);
      scroll = (0.0, (i as f32) * 100.0);
      throttle.observe_scroll_change(now);
      if throttle.tick(now, || std::iter::once((tab, scroll))) {
        saves += 1;
      }
    }

    assert_eq!(saves, 1);
  }

  #[test]
  fn idle_period_triggers_final_save() {
    let cfg = ScrollAutosaveConfig {
      interval: Duration::from_secs(2),
      debounce: Duration::from_millis(750),
      threshold_css: 128.0,
    };
    let mut throttle = ScrollAutosaveThrottle::new(cfg);
    let tab = TabId(1);
    throttle.set_tab_baseline_scroll_css(tab, (0.0, 0.0));

    let start = Instant::now();
    let mut scroll = (0.0f32, 0.0f32);

    // A short scroll burst (< interval) should not trigger the periodic save, but it should trigger
    // one save shortly after scrolling stops (debounce).
    for i in 0..=2u64 {
      let now = start + Duration::from_millis(i * 100);
      scroll = (0.0, 200.0 + (i as f32) * 200.0);
      throttle.observe_scroll_change(now);
      assert!(!throttle.tick(now, || std::iter::once((tab, scroll))));
    }

    // After the debounce window elapses, a final autosave should be requested.
    let now = start + Duration::from_millis(1200);
    assert!(throttle.tick(now, || std::iter::once((tab, scroll))));
  }

  #[test]
  fn small_scroll_deltas_below_threshold_do_not_trigger_saves() {
    let cfg = ScrollAutosaveConfig {
      interval: Duration::from_secs(2),
      debounce: Duration::from_millis(750),
      threshold_css: 128.0,
    };
    let mut throttle = ScrollAutosaveThrottle::new(cfg);
    let tab = TabId(1);
    throttle.set_tab_baseline_scroll_css(tab, (0.0, 0.0));

    let start = Instant::now();
    let scroll = (0.0f32, 50.0f32);
    throttle.observe_scroll_change(start);
    assert!(!throttle.tick(start, || std::iter::once((tab, scroll))));

    // Wait long enough for the debounce window; the delta is still below threshold, so no save.
    let later = start + Duration::from_millis(1000);
    assert!(!throttle.tick(later, || std::iter::once((tab, scroll))));
  }
}

