//! Minimal touch gesture recognizer used by the windowed browser UI.
//!
//! This module intentionally avoids any `winit`/`egui` types so it can be unit tested without the
//! heavy `browser_ui` feature.

use std::time::{Duration, Instant};

/// A high-level action produced by [`TouchGestureRecognizer`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchGestureAction {
  /// A long-press gesture was recognised and should open a page context menu.
  ContextMenu { pos_points: (f32, f32) },
  /// A tap gesture was recognised and should be treated as a primary click.
  Tap { pos_points: (f32, f32) },
}

/// Minimal touch gesture recogniser used to emulate mouse interactions on touch devices.
#[derive(Debug, Clone)]
pub struct TouchGestureRecognizer {
  active: Option<ActiveTouchGesture>,
  long_press_threshold: Duration,
  slop_radius_points: f32,
}

#[derive(Debug, Clone)]
struct ActiveTouchGesture {
  id: u64,
  start_pos_points: (f32, f32),
  last_pos_points: (f32, f32),
  #[allow(dead_code)]
  start_instant: Instant,
  long_press_deadline: Instant,
  moved_too_far: bool,
  long_press_triggered: bool,
}

impl TouchGestureRecognizer {
  pub const DEFAULT_LONG_PRESS_THRESHOLD: Duration = Duration::from_millis(500);
  pub const DEFAULT_SLOP_RADIUS_POINTS: f32 = 8.0;

  #[must_use]
  pub fn new() -> Self {
    Self::new_with_config(
      Self::DEFAULT_LONG_PRESS_THRESHOLD,
      Self::DEFAULT_SLOP_RADIUS_POINTS,
    )
  }

  #[must_use]
  pub fn new_with_config(long_press_threshold: Duration, slop_radius_points: f32) -> Self {
    Self {
      active: None,
      long_press_threshold,
      slop_radius_points: slop_radius_points.max(0.0),
    }
  }

  pub fn reset(&mut self) {
    self.active = None;
  }

  #[must_use]
  pub fn next_deadline(&self) -> Option<Instant> {
    self.active.as_ref().and_then(|gesture| {
      if gesture.moved_too_far || gesture.long_press_triggered {
        None
      } else {
        Some(gesture.long_press_deadline)
      }
    })
  }

  pub fn touch_start(&mut self, id: u64, now: Instant, pos_points: (f32, f32)) {
    // Multi-touch is treated as a cancellation of the primary-touch gesture so we don't trigger
    // context menus during pinch/zoom interactions.
    if self.active.is_some() {
      self.active = None;
    }
    self.active = Some(ActiveTouchGesture {
      id,
      start_pos_points: pos_points,
      last_pos_points: pos_points,
      start_instant: now,
      long_press_deadline: now + self.long_press_threshold,
      moved_too_far: false,
      long_press_triggered: false,
    });
  }

  /// Update the active touch gesture with a new position.
  ///
  /// When the gesture transitions into a drag (movement exceeds the configured slop radius), this
  /// returns the most recent finger delta (in points) so callers can translate it into a scroll
  /// delta.
  pub fn touch_move(&mut self, id: u64, pos_points: (f32, f32)) -> Option<(f32, f32)> {
    let Some(active) = self.active.as_mut() else {
      return None;
    };
    if active.id != id {
      return None;
    }

    let prev_pos_points = active.last_pos_points;
    active.last_pos_points = pos_points;
    let delta_points = (
      pos_points.0 - prev_pos_points.0,
      pos_points.1 - prev_pos_points.1,
    );
    if active.long_press_triggered {
      return None;
    }

    if !active.moved_too_far {
      let dx = pos_points.0 - active.start_pos_points.0;
      let dy = pos_points.1 - active.start_pos_points.1;
      let dist2 = dx * dx + dy * dy;
      let max_dist2 = self.slop_radius_points * self.slop_radius_points;
      if dist2 > max_dist2 {
        active.moved_too_far = true;
      }
    }

    if active.moved_too_far && (delta_points.0 != 0.0 || delta_points.1 != 0.0) {
      Some(delta_points)
    } else {
      None
    }
  }

  #[must_use]
  pub fn touch_end(
    &mut self,
    id: u64,
    now: Instant,
    pos_points: (f32, f32),
  ) -> Option<TouchGestureAction> {
    let Some(active) = self.active.take() else {
      return None;
    };
    if active.id != id {
      // Different touch ended; keep the original gesture (best-effort) by restoring it.
      self.active = Some(active);
      return None;
    }

    if active.moved_too_far {
      return None;
    }

    if active.long_press_triggered {
      // Long-press was already emitted via `tick`; do not re-emit on release.
      return None;
    }

    if now >= active.long_press_deadline {
      // Treat a release after the deadline as a long-press even if the periodic tick didn't fire
      // (e.g. event-loop scheduling delays).
      return Some(TouchGestureAction::ContextMenu { pos_points });
    }

    Some(TouchGestureAction::Tap { pos_points })
  }

  pub fn touch_cancel(&mut self, id: u64) {
    if self.active.as_ref().is_some_and(|active| active.id == id) {
      self.active = None;
    }
  }

  #[must_use]
  pub fn tick(&mut self, now: Instant) -> Option<TouchGestureAction> {
    let Some(active) = self.active.as_mut() else {
      return None;
    };
    if active.moved_too_far || active.long_press_triggered {
      return None;
    }
    if now < active.long_press_deadline {
      return None;
    }

    active.long_press_triggered = true;
    Some(TouchGestureAction::ContextMenu {
      pos_points: active.last_pos_points,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{Duration, Instant};

  #[test]
  fn held_past_threshold_triggers_context_menu() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (10.0, 20.0));
    assert_eq!(recognizer.tick(t0 + Duration::from_millis(499)), None);
    assert_eq!(
      recognizer.tick(t0 + Duration::from_millis(500)),
      Some(TouchGestureAction::ContextMenu {
        pos_points: (10.0, 20.0)
      })
    );
  }

  #[test]
  fn movement_beyond_slop_cancels_long_press() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (0.0, 0.0));
    recognizer.touch_move(1, (10.0, 0.0));
    assert_eq!(recognizer.tick(t0 + Duration::from_millis(600)), None);
  }

  #[test]
  fn long_press_suppresses_tap_on_release() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (5.0, 5.0));
    assert!(matches!(
      recognizer.tick(t0 + Duration::from_millis(600)),
      Some(TouchGestureAction::ContextMenu { .. })
    ));
    assert_eq!(
      recognizer.touch_end(1, t0 + Duration::from_millis(650), (5.0, 5.0)),
      None
    );
  }

  #[test]
  fn tap_returns_tap_action() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (10.0, 20.0));
    assert_eq!(
      recognizer.touch_end(1, t0 + Duration::from_millis(20), (10.0, 20.0)),
      Some(TouchGestureAction::Tap {
        pos_points: (10.0, 20.0)
      })
    );
  }

  #[test]
  fn small_jitter_still_counts_as_tap() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (0.0, 0.0));
    // Within slop radius: should not be treated as a drag.
    assert_eq!(recognizer.touch_move(1, (3.0, 0.0)), None);
    assert_eq!(
      recognizer.touch_end(1, t0 + Duration::from_millis(20), (3.0, 0.0)),
      Some(TouchGestureAction::Tap {
        pos_points: (3.0, 0.0)
      })
    );
  }

  #[test]
  fn drag_emits_scroll_deltas_and_suppresses_tap() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (0.0, 0.0));

    // Move beyond slop radius: should be treated as a drag and return finger deltas.
    assert_eq!(recognizer.touch_move(1, (0.0, 10.0)), Some((0.0, 10.0)));
    assert_eq!(recognizer.touch_move(1, (0.0, 18.0)), Some((0.0, 8.0)));

    // Drag gestures should not trigger long-press context menus.
    assert_eq!(recognizer.tick(t0 + Duration::from_millis(600)), None);

    // Drag gestures should not be synthesised as a click on release.
    assert_eq!(
      recognizer.touch_end(1, t0 + Duration::from_millis(650), (0.0, 18.0)),
      None
    );
  }
}
