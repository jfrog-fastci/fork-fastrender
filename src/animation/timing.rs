//! Web Animations (WA1) timing primitives.
//!
//! This module provides a minimal, spec-shaped timing model that is suitable for
//! integrating an animation engine later. It is intentionally standalone and is
//! not yet wired into CSS animation sampling.
//!
//! References:
//! - Web Animations 1: https://www.w3.org/TR/web-animations-1/
//!   - "Document timelines"
//!   - "Calculating the current time of an animation" (Overview.bs §2.4)
//!   - "Setting the playback rate"

/// A time value in milliseconds that can be resolved or unresolved.
///
/// Web Animations allows negative time values, so we use `f64` rather than
/// `std::time::Duration`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeValue(Option<f64>);

impl TimeValue {
  pub const UNRESOLVED: Self = Self(None);

  /// Create a resolved time value in milliseconds.
  ///
  /// Non-finite values (NaN/±inf) are treated as unresolved.
  pub fn resolved(millis: f64) -> Self {
    if millis.is_finite() {
      Self(Some(millis))
    } else {
      Self::UNRESOLVED
    }
  }

  pub fn is_resolved(self) -> bool {
    self.0.is_some()
  }

  pub fn is_unresolved(self) -> bool {
    self.0.is_none()
  }

  pub fn as_millis(self) -> Option<f64> {
    self.0
  }

  fn checked_add(self, other: Self) -> Self {
    let Some(a) = self.0 else {
      return Self::UNRESOLVED;
    };
    let Some(b) = other.0 else {
      return Self::UNRESOLVED;
    };
    Self::resolved(a + b)
  }

  fn checked_sub(self, other: Self) -> Self {
    let Some(a) = self.0 else {
      return Self::UNRESOLVED;
    };
    let Some(b) = other.0 else {
      return Self::UNRESOLVED;
    };
    Self::resolved(a - b)
  }

  fn checked_mul_f64(self, factor: f64) -> Self {
    if !factor.is_finite() {
      return Self::UNRESOLVED;
    }
    let Some(v) = self.0 else {
      return Self::UNRESOLVED;
    };
    Self::resolved(v * factor)
  }

  fn checked_div_f64(self, denom: f64) -> Self {
    if !denom.is_finite() || denom == 0.0 {
      return Self::UNRESOLVED;
    }
    let Some(v) = self.0 else {
      return Self::UNRESOLVED;
    };
    Self::resolved(v / denom)
  }
}

/// A document timeline as defined by Web Animations.
///
/// `now`/`origin_time` are expressed in the same monotonic time coordinate
/// system (e.g. milliseconds since a process start `Instant`). `current_time` is
/// the origin-relative time (`now - origin_time`).
#[derive(Debug, Clone)]
pub struct DocumentTimeline {
  origin_time: f64,
  current_time: TimeValue,
  active: bool,
}

impl DocumentTimeline {
  pub fn new(origin_time: f64) -> Self {
    Self {
      origin_time: if origin_time.is_finite() { origin_time } else { 0.0 },
      current_time: TimeValue::UNRESOLVED,
      active: origin_time.is_finite(),
    }
  }

  pub fn is_active(&self) -> bool {
    self.active
  }

  pub fn set_active(&mut self, active: bool) {
    self.active = active;
    if !active {
      self.current_time = TimeValue::UNRESOLVED;
    }
  }

  pub fn origin_time(&self) -> f64 {
    self.origin_time
  }

  pub fn current_time(&self) -> TimeValue {
    if self.active {
      self.current_time
    } else {
      TimeValue::UNRESOLVED
    }
  }

  /// Update `current_time` using the monotonic `now` input.
  ///
  /// When inactive, the timeline's `current_time` becomes unresolved.
  pub fn update(&mut self, now: f64) {
    if !self.active || !self.origin_time.is_finite() || !now.is_finite() {
      self.current_time = TimeValue::UNRESOLVED;
      return;
    }
    self.current_time = TimeValue::resolved(now - self.origin_time);
  }

  /// Convert a timeline time (relative to the timeline origin) back into the
  /// origin-relative coordinate system.
  ///
  /// This corresponds to the WA1 `timeline time to origin-relative time`
  /// conversion.
  pub fn timeline_time_to_origin_relative_time(&self, time: TimeValue) -> TimeValue {
    if !self.origin_time.is_finite() {
      return TimeValue::UNRESOLVED;
    }
    time.checked_add(TimeValue::resolved(self.origin_time))
  }
}

/// Minimal state for WA1 "AnimationPlayer" timekeeping.
#[derive(Debug, Clone)]
pub struct AnimationTimingState {
  start_time: TimeValue,
  hold_time: TimeValue,
  playback_rate: f64,
}

impl Default for AnimationTimingState {
  fn default() -> Self {
    Self {
      start_time: TimeValue::UNRESOLVED,
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
    }
  }
}

impl AnimationTimingState {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn start_time(&self) -> TimeValue {
    self.start_time
  }

  pub fn hold_time(&self) -> TimeValue {
    self.hold_time
  }

  pub fn playback_rate(&self) -> f64 {
    self.playback_rate
  }

  /// WA1: Calculating the current time of an animation (Overview.bs §2.4).
  pub fn current_time_at_timeline_time(&self, timeline_time: TimeValue) -> TimeValue {
    // 1. If the hold time is resolved, return it.
    if self.hold_time.is_resolved() {
      return self.hold_time;
    }

    // 2. If there is no timeline / inactive timeline / start time unresolved,
    //    return unresolved. We model "no/inactive timeline" by passing an
    //    unresolved `timeline_time`.
    if timeline_time.is_unresolved() {
      return TimeValue::UNRESOLVED;
    };
    if self.start_time.is_unresolved() {
      return TimeValue::UNRESOLVED;
    };

    // 3. Otherwise, return (timeline_time - start_time) * playback_rate.
    timeline_time
      .checked_sub(self.start_time)
      .checked_mul_f64(self.playback_rate)
  }

  pub fn current_time(&self, timeline: Option<&DocumentTimeline>) -> TimeValue {
    let timeline_time = timeline.map_or(TimeValue::UNRESOLVED, |t| t.current_time());
    self.current_time_at_timeline_time(timeline_time)
  }

  pub fn pause(&mut self, timeline_time: TimeValue) {
    let current = self.current_time_at_timeline_time(timeline_time);
    self.hold_time = current;
    self.start_time = TimeValue::UNRESOLVED;
  }

  pub fn play(&mut self, timeline_time: TimeValue) {
    // Resume from a resolved hold time.
    if self.hold_time.is_resolved() && self.playback_rate != 0.0 {
      if timeline_time.is_unresolved() {
        // Timeline is inactive/unresolved; preserve the hold time until we can
        // resolve a start time.
        return;
      };
      let start_time = timeline_time.checked_sub(self.hold_time.checked_div_f64(self.playback_rate));
      if start_time.is_resolved() {
        self.start_time = start_time;
        self.hold_time = TimeValue::UNRESOLVED;
      }
      return;
    }

    // Start playing from current time 0 when we have no existing timing
    // information.
    if self.start_time.is_unresolved() && self.hold_time.is_unresolved() && timeline_time.is_resolved() {
      self.start_time = timeline_time;
    }
  }

  /// Simplified WA1 "silently set current time" logic.
  ///
  /// This updates either the hold time (paused/idle or inactive timeline) or
  /// the start time (playing) depending on the current state.
  pub fn set_current_time(&mut self, seek_time: TimeValue, timeline_time: TimeValue) {
    // Treat invalid input as an unresolved seek.
    if seek_time.is_unresolved() {
      self.hold_time = TimeValue::UNRESOLVED;
      self.start_time = TimeValue::UNRESOLVED;
      return;
    }

    let is_playing =
      self.hold_time.is_unresolved() && self.start_time.is_resolved() && timeline_time.is_resolved() && self.playback_rate != 0.0;

    if is_playing {
      // start_time = timeline_time - seek_time / playback_rate
      let start_time = timeline_time.checked_sub(seek_time.checked_div_f64(self.playback_rate));
      if start_time.is_resolved() {
        self.start_time = start_time;
      } else {
        // Fall back to a hold time if the math produces a non-finite result.
        self.hold_time = seek_time;
        self.start_time = TimeValue::UNRESOLVED;
      }
    } else {
      // When paused/idle (or timeline is inactive), store a hold time and clear
      // the start time.
      self.hold_time = seek_time;
      self.start_time = TimeValue::UNRESOLVED;
    }
  }

  /// WA1: Setting the playback rate (monotonic timeline branch).
  ///
  /// `timeline_is_monotonic` is `true` for document timelines. For non-monotonic
  /// timelines (e.g. scroll-driven timelines), WA1 specifies additional
  /// machinery involving pending playback rates which is not implemented yet.
  pub fn set_playback_rate(&mut self, new_rate: f64, timeline_time: TimeValue, timeline_is_monotonic: bool) {
    if !new_rate.is_finite() {
      return;
    }

    if !timeline_is_monotonic {
      // Limitation: WA1 defines additional behavior for non-monotonic timelines
      // and pending playback rates. For now we update the rate without trying
      // to preserve current time.
      self.playback_rate = new_rate;
      return;
    }

    let current = self.current_time_at_timeline_time(timeline_time);
    self.playback_rate = new_rate;

    if current.is_resolved() {
      self.set_current_time(current, timeline_time);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn current_time_prefers_hold_time() {
    let state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::resolved(123.0),
      playback_rate: 1.0,
    };

    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(999.0)),
      TimeValue::resolved(123.0)
    );
  }

  #[test]
  fn current_time_unresolved_when_start_time_unresolved() {
    let state = AnimationTimingState::new();
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(10.0)),
      TimeValue::UNRESOLVED
    );
  }

  #[test]
  fn current_time_scales_by_playback_rate() {
    let state = AnimationTimingState {
      start_time: TimeValue::resolved(10.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 2.0,
    };
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(20.0)),
      TimeValue::resolved(20.0)
    );
  }

  #[test]
  fn pause_then_play_preserves_current_time_progression() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
    };

    // Playing at t=50 => currentTime=50.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(50.0)),
      TimeValue::resolved(50.0)
    );

    // Pause freezes the current time.
    state.pause(TimeValue::resolved(50.0));
    assert_eq!(state.hold_time(), TimeValue::resolved(50.0));
    assert!(state.start_time().is_unresolved());
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(80.0)),
      TimeValue::resolved(50.0)
    );

    // Play at t=80 resumes such that currentTime is preserved at the play
    // moment.
    state.play(TimeValue::resolved(80.0));
    assert!(state.hold_time().is_unresolved());
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(80.0)),
      TimeValue::resolved(50.0)
    );

    // Time continues to advance from the paused point.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(100.0)),
      TimeValue::resolved(70.0)
    );
  }

  #[test]
  fn set_playback_rate_preserves_current_time_for_monotonic_timelines() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
    };

    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(50.0)),
      TimeValue::resolved(50.0)
    );

    state.set_playback_rate(2.0, TimeValue::resolved(50.0), true);

    // The current time at the moment of change is preserved.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(50.0)),
      TimeValue::resolved(50.0)
    );

    // It now advances twice as fast.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(60.0)),
      TimeValue::resolved(70.0)
    );
  }
}
