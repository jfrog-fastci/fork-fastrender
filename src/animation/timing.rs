//! Web Animations (WA1) timing primitives.
//!
//! This module provides a minimal, spec-shaped timing model used by FastRender's
//! CSS animation/transition sampling pipeline for deterministic, single-frame
//! renders as well as multi-frame "pause/resume" state.
//!
//! References:
//! - Web Animations 1: <https://www.w3.org/TR/web-animations-1/>
//!   - "Document timelines"
//!   - "Calculating the current time of an animation" (Overview.bs §2.4)
//!   - "Setting the playback rate"

use crate::style::types::{AnimationDirection, AnimationFillMode};

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
      origin_time: if origin_time.is_finite() {
        origin_time
      } else {
        0.0
      },
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
  timeline_time: TimeValue,
  start_time: TimeValue,
  hold_time: TimeValue,
  playback_rate: f64,
  pending_playback_rate: Option<f64>,
  previous_current_time: TimeValue,
}

impl Default for AnimationTimingState {
  fn default() -> Self {
    Self {
      timeline_time: TimeValue::UNRESOLVED,
      start_time: TimeValue::UNRESOLVED,
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      pending_playback_rate: None,
      previous_current_time: TimeValue::UNRESOLVED,
    }
  }
}

impl AnimationTimingState {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn set_timeline_time(&mut self, timeline_time: TimeValue) {
    self.timeline_time = timeline_time;
    // Apply a deferred playback-rate change once we have timeline time to
    // anchor it.
    self.apply_pending_playback_rate_preserving_current_time(timeline_time);
  }

  pub fn timeline_time(&self) -> TimeValue {
    self.timeline_time
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

  pub fn previous_current_time(&self) -> TimeValue {
    self.previous_current_time
  }

  fn apply_pending_playback_rate_preserving_current_time(&mut self, timeline_time: TimeValue) {
    let Some(pending) = self.pending_playback_rate else {
      return;
    };

    // If we already have a resolved hold time, the animation's current time is
    // anchored and does not depend on the playback rate. In this state we
    // should avoid calling `set_current_time`, which would clear a resolved
    // `start_time` (used by finished animations to un-finish on non-monotonic
    // timelines).
    if self.hold_time.is_resolved() {
      // Match the WA1 "finished" play state branch by preserving the
      // unconstrained current time (ignoring the hold time) when possible.
      if self.start_time.is_resolved() && timeline_time.is_resolved() {
        if pending == 0.0 {
          self.start_time = timeline_time;
        } else {
          let unconstrained =
            self.calculate_current_time_at_timeline_time(timeline_time, /* ignore_hold_time */ true);
          if unconstrained.is_resolved() {
            let start_time =
              timeline_time.checked_sub(unconstrained.checked_div_f64(pending));
            if start_time.is_resolved() {
              self.start_time = start_time;
            }
          }
        }
      }
      self.playback_rate = pending;
      self.pending_playback_rate = None;
      return;
    }

    // Calculate the current time using the existing playback rate so we can
    // preserve it once the pending rate is applied.
    let current = self.current_time_at_timeline_time(timeline_time);
    self.playback_rate = pending;
    self.pending_playback_rate = None;

    if current.is_resolved() {
      self.set_current_time(current, timeline_time);
    }
  }

  fn calculate_current_time_at_timeline_time(
    &self,
    timeline_time: TimeValue,
    ignore_hold_time: bool,
  ) -> TimeValue {
    // 1. If the hold time is resolved, return it.
    if !ignore_hold_time && self.hold_time.is_resolved() {
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

  fn calculate_current_time(&self, ignore_hold_time: bool) -> TimeValue {
    self.calculate_current_time_at_timeline_time(self.timeline_time, ignore_hold_time)
  }

  fn timeline_is_active(&self) -> bool {
    self.timeline_time.is_resolved()
  }

  /// WA1: Calculating the current time of an animation (Overview.bs §2.4).
  pub fn current_time_at_timeline_time(&self, timeline_time: TimeValue) -> TimeValue {
    self.calculate_current_time_at_timeline_time(timeline_time, /* ignore_hold_time */ false)
  }

  pub fn current_time(&self, timeline: Option<&DocumentTimeline>) -> TimeValue {
    let timeline_time = timeline.map_or(TimeValue::UNRESOLVED, |t| t.current_time());
    self.current_time_at_timeline_time(timeline_time)
  }

  pub fn pause(&mut self, timeline_time: TimeValue) {
    self.timeline_time = timeline_time;
    self.apply_pending_playback_rate_preserving_current_time(timeline_time);
    let current = self.current_time_at_timeline_time(timeline_time);
    self.hold_time = current;
    self.start_time = TimeValue::UNRESOLVED;
  }

  pub fn play(&mut self, timeline_time: TimeValue) {
    self.timeline_time = timeline_time;
    self.apply_pending_playback_rate_preserving_current_time(timeline_time);
    // Resume from a resolved hold time.
    if self.hold_time.is_resolved() && self.playback_rate != 0.0 {
      if timeline_time.is_unresolved() {
        // Timeline is inactive/unresolved; preserve the hold time until we can
        // resolve a start time.
        return;
      };
      let start_time =
        timeline_time.checked_sub(self.hold_time.checked_div_f64(self.playback_rate));
      if start_time.is_resolved() {
        self.start_time = start_time;
        self.hold_time = TimeValue::UNRESOLVED;
      }
      return;
    }

    // Start playing from current time 0 when we have no existing timing
    // information.
    if self.start_time.is_unresolved()
      && self.hold_time.is_unresolved()
      && timeline_time.is_resolved()
    {
      self.start_time = timeline_time;
    }
  }

  /// Simplified WA1 "silently set current time" logic.
  ///
  /// This updates either the hold time (paused/idle or inactive timeline) or
  /// the start time (playing) depending on the current state.
  pub fn set_current_time(&mut self, seek_time: TimeValue, timeline_time: TimeValue) {
    self.timeline_time = timeline_time;
    self.apply_pending_playback_rate_preserving_current_time(timeline_time);
    // Treat invalid input as an unresolved seek.
    if seek_time.is_unresolved() {
      self.hold_time = TimeValue::UNRESOLVED;
      self.start_time = TimeValue::UNRESOLVED;
      return;
    }

    let is_playing = self.hold_time.is_unresolved()
      && self.start_time.is_resolved()
      && timeline_time.is_resolved()
      && self.playback_rate != 0.0;

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

  /// WA1: Setting the playback rate.
  ///
  /// `timeline_is_monotonic` is `true` for document timelines. For non-monotonic
  /// timelines (e.g. scroll-driven timelines), we use the WA1 pending playback
  /// rate machinery to preserve the current time at the moment of the rate
  /// change.
  pub fn set_playback_rate(
    &mut self,
    new_rate: f64,
    timeline_time: TimeValue,
    timeline_is_monotonic: bool,
  ) {
    self.timeline_time = timeline_time;
    if !new_rate.is_finite() {
      return;
    }

    if !timeline_is_monotonic {
      self.pending_playback_rate = Some(new_rate);
      self.apply_pending_playback_rate_preserving_current_time(timeline_time);
      return;
    }

    // A direct playbackRate set clears any pending rate update.
    self.pending_playback_rate = None;
    let current = self.current_time_at_timeline_time(timeline_time);
    self.playback_rate = new_rate;

    if current.is_resolved() {
      self.set_current_time(current, timeline_time);
    }
  }

  /// WA1: Update an animation's finished state (timing-only).
  ///
  /// This corresponds to the "Update an animation's finished state" algorithm in WA1
  /// (Overview.bs §2.8), limited to the hold-time clamping logic. Promise/job queue
  /// and event dispatch are handled elsewhere.
  pub fn update_finished_state(&mut self, associated_effect_end: f64, did_seek: bool) {
    let unconstrained_current_time = if did_seek {
      self.calculate_current_time(/* ignore_hold_time */ false)
    } else {
      self.calculate_current_time(/* ignore_hold_time */ true)
    };

    if unconstrained_current_time.is_resolved() && self.start_time.is_resolved() {
      let unconstrained = unconstrained_current_time.as_millis().unwrap_or(0.0);

      if self.playback_rate > 0.0 && unconstrained >= associated_effect_end {
        self.hold_time = if did_seek {
          TimeValue::resolved(unconstrained)
        } else {
          let prev = self
            .previous_current_time
            .as_millis()
            .unwrap_or(associated_effect_end);
          TimeValue::resolved(prev.max(associated_effect_end))
        };
      } else if self.playback_rate < 0.0 && unconstrained <= 0.0 {
        self.hold_time = if did_seek {
          TimeValue::resolved(unconstrained)
        } else {
          let prev = self.previous_current_time.as_millis().unwrap_or(0.0);
          TimeValue::resolved(prev.min(0.0))
        };
      } else if self.playback_rate != 0.0 && self.timeline_is_active() {
        self.hold_time = TimeValue::UNRESOLVED;
      }
    }

    self.previous_current_time = self.calculate_current_time(/* ignore_hold_time */ false);
  }
}

/// A minimal "computed timing" result used by the CSS animation/transition
/// sampling code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EffectProgress {
  pub(crate) progress: f32,
  pub(crate) iteration: u64,
}

#[inline]
pub(crate) fn fill_backwards(fill: AnimationFillMode) -> bool {
  matches!(fill, AnimationFillMode::Backwards | AnimationFillMode::Both)
}

#[inline]
pub(crate) fn fill_forwards(fill: AnimationFillMode) -> bool {
  matches!(fill, AnimationFillMode::Forwards | AnimationFillMode::Both)
}

#[inline]
pub(crate) fn iteration_reverses(direction: AnimationDirection, iteration: u64) -> bool {
  match direction {
    AnimationDirection::Normal => false,
    AnimationDirection::Reverse => true,
    AnimationDirection::Alternate => iteration % 2 == 1,
    AnimationDirection::AlternateReverse => iteration % 2 == 0,
  }
}

pub(crate) fn animation_end_progress(direction: AnimationDirection, iterations: f32) -> f32 {
  if !iterations.is_finite() {
    return 0.0;
  }
  let iterations = iterations.max(0.0);
  let reversed_start = iteration_reverses(direction, 0);
  if iterations <= 0.0 {
    return if reversed_start { 1.0 } else { 0.0 };
  }

  let whole = iterations.floor();
  let frac = (iterations - whole).clamp(0.0, 1.0);
  if frac <= f32::EPSILON {
    let last_iteration = (whole.max(1.0) as u64).saturating_sub(1);
    let reversed = iteration_reverses(direction, last_iteration);
    if reversed {
      0.0
    } else {
      1.0
    }
  } else {
    let iteration = whole as u64;
    let reversed = iteration_reverses(direction, iteration);
    if reversed {
      1.0 - frac
    } else {
      frac
    }
  }
}

pub(crate) fn animation_end_iteration(iterations: f32) -> u64 {
  if !iterations.is_finite() {
    return 0;
  }
  let iterations = iterations.max(0.0);
  if iterations <= 0.0 {
    return 0;
  }
  let whole = iterations.floor();
  let frac = (iterations - whole).clamp(0.0, 1.0);
  if frac <= f32::EPSILON {
    (whole.max(1.0) as u64).saturating_sub(1)
  } else {
    whole as u64
  }
}

/// Sample a CSS keyframe effect at a given WA1 `currentTime` (milliseconds).
///
/// This implements a subset of WA1 computed timing sufficient for FastRender's
/// CSS animation and transition engines:
/// - start delays (including negative)
/// - iteration count/duration/direction
/// - fill modes (`none|backwards|forwards|both`)
///
/// The returned progress is the *directed* iteration progress in the range
/// [0,1], before applying any easing functions.
pub(crate) fn sample_css_animation_effect(
  current_time: TimeValue,
  delay_ms: f32,
  duration_ms: f32,
  iterations: f32,
  direction: AnimationDirection,
  fill: AnimationFillMode,
) -> Option<EffectProgress> {
  let current_ms = current_time.as_millis()? as f32;
  if !(current_ms.is_finite()
    && delay_ms.is_finite()
    && duration_ms.is_finite()
    && (iterations.is_finite() || iterations.is_infinite()))
  {
    return None;
  }

  let duration = duration_ms.max(0.0);
  let delay = delay_ms;
  let local_time = current_ms - delay;

  let active_duration = if duration <= 0.0 {
    0.0
  } else if iterations.is_infinite() {
    f32::INFINITY
  } else {
    duration * iterations.max(0.0)
  };

  let start_progress = if iteration_reverses(direction, 0) {
    1.0
  } else {
    0.0
  };
  let end_progress = animation_end_progress(direction, iterations);
  let end_iteration = animation_end_iteration(iterations);

  if local_time < 0.0 {
    return fill_backwards(fill).then_some(EffectProgress {
      progress: start_progress,
      iteration: 0,
    });
  }

  if active_duration.is_finite() && local_time >= active_duration {
    return fill_forwards(fill).then_some(EffectProgress {
      progress: end_progress,
      iteration: end_iteration,
    });
  }

  if duration <= 0.0 {
    // Active duration is 0, so we can only reach this point if it is infinite
    // (which can happen for non-finite inputs we reject above). Keep this as a
    // defensive fallback.
    return Some(EffectProgress {
      progress: end_progress,
      iteration: end_iteration,
    });
  }

  let total = local_time / duration;
  if !total.is_finite() {
    return None;
  }

  let iteration = total.floor() as u64;
  let iteration_progress = (total - iteration as f32).clamp(0.0, 1.0);
  let reversed = iteration_reverses(direction, iteration);
  let directed = if reversed {
    1.0 - iteration_progress
  } else {
    iteration_progress
  };

  Some(EffectProgress {
    progress: directed,
    iteration,
  })
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
      ..AnimationTimingState::new()
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
      ..AnimationTimingState::new()
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
      ..AnimationTimingState::new()
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
      ..AnimationTimingState::new()
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

  #[test]
  fn set_playback_rate_preserves_current_time_for_non_monotonic_timelines() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);

    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);

    state.set_playback_rate(2.0, at_50, false);

    // The current time at the moment of change is preserved.
    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);

    // It now advances twice as fast.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(60.0)),
      TimeValue::resolved(70.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_timeline_time_can_go_backwards() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);
    state.set_playback_rate(2.0, at_50, false);

    // If the timeline time jumps backwards after the rate change, currentTime
    // is still computed from the preserved reference point.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(40.0)),
      TimeValue::resolved(30.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_negative_rate_preserves_current_time() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);
    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);

    state.set_playback_rate(-1.0, at_50, false);

    // The current time at the moment of change is preserved.
    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);

    // Time now runs backwards as the timeline increases.
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(60.0)),
      TimeValue::resolved(40.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_while_paused_keeps_hold_time() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);
    state.pause(at_50);
    assert_eq!(state.hold_time(), at_50);

    // Change playback rate while paused.
    state.set_playback_rate(2.0, TimeValue::resolved(60.0), false);
    assert_eq!(state.hold_time(), at_50);

    // Resuming uses the new rate while preserving the paused time.
    state.play(TimeValue::resolved(80.0));
    assert_eq!(state.current_time_at_timeline_time(TimeValue::resolved(80.0)), at_50);
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(90.0)),
      TimeValue::resolved(70.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_finished_state_preserves_start_time_for_unfinish() {
    let mut state = AnimationTimingState {
      // Model a finished animation at effect-end=100ms with a start time of 0ms.
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::resolved(100.0),
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };

    let at_200 = TimeValue::resolved(200.0);
    state.set_playback_rate(2.0, at_200, false);

    // Preserve the unconstrained current time (ignoring hold_time) by updating
    // the start time: 200 - (200 / 2) = 100.
    assert_eq!(state.start_time(), TimeValue::resolved(100.0));
    assert_eq!(state.hold_time(), TimeValue::resolved(100.0));

    // Move backwards past the end boundary. With the adjusted start time, the
    // unconstrained current time at t=149 is 98, so the animation should leave
    // the finished state.
    state.set_timeline_time(TimeValue::resolved(149.0));
    state.update_finished_state(/* associated_effect_end */ 100.0, /* did_seek */ false);
    assert!(state.hold_time().is_unresolved());
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::resolved(149.0)),
      TimeValue::resolved(98.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_zero_rate_freezes_current_time() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);
    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);

    state.set_playback_rate(0.0, at_50, false);

    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);
    assert_eq!(state.current_time_at_timeline_time(TimeValue::resolved(60.0)), at_50);
    assert_eq!(state.current_time_at_timeline_time(TimeValue::resolved(40.0)), at_50);
    assert_eq!(state.hold_time(), at_50);
    assert!(state.start_time().is_unresolved());
  }

  #[test]
  fn set_playback_rate_non_monotonic_ignores_non_finite_input() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    let at_50 = TimeValue::resolved(50.0);
    state.set_playback_rate(f64::NAN, at_50, false);
    assert_eq!(state.playback_rate(), 1.0);
    assert_eq!(state.current_time_at_timeline_time(at_50), at_50);
  }

  #[test]
  fn set_playback_rate_non_monotonic_unresolved_timeline_time_keeps_hold_time() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::UNRESOLVED,
      hold_time: TimeValue::resolved(20.0),
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    state.set_playback_rate(2.0, TimeValue::UNRESOLVED, false);
    assert_eq!(state.playback_rate(), 2.0);
    assert_eq!(state.hold_time(), TimeValue::resolved(20.0));
    assert!(state.start_time().is_unresolved());
    assert_eq!(
      state.current_time_at_timeline_time(TimeValue::UNRESOLVED),
      TimeValue::resolved(20.0)
    );
  }

  #[test]
  fn set_playback_rate_non_monotonic_unresolved_timeline_time_updates_playback_rate() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      hold_time: TimeValue::UNRESOLVED,
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };

    state.set_playback_rate(2.0, TimeValue::UNRESOLVED, false);

    // With an inactive/unresolved timeline, the current time is unresolved so we
    // cannot preserve it, but the new playback rate should still be applied.
    assert_eq!(state.playback_rate(), 2.0);
    assert_eq!(state.pending_playback_rate, None);
    assert!(state.current_time_at_timeline_time(TimeValue::UNRESOLVED).is_unresolved());
  }

  #[test]
  fn finished_state_playback_rate_positive_crossing_end_clamps_to_end() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    state.set_timeline_time(TimeValue::resolved(12.0));

    state.update_finished_state(
      /* associated_effect_end */ 10.0, /* did_seek */ false,
    );

    assert_eq!(state.hold_time(), TimeValue::resolved(10.0));
    assert_eq!(state.previous_current_time(), TimeValue::resolved(10.0));
  }

  #[test]
  fn finished_state_playback_rate_negative_crossing_zero_clamps_to_zero() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(10.0),
      playback_rate: -1.0,
      ..AnimationTimingState::new()
    };
    state.set_timeline_time(TimeValue::resolved(12.0));

    state.update_finished_state(
      /* associated_effect_end */ 10.0, /* did_seek */ false,
    );

    assert_eq!(state.hold_time(), TimeValue::resolved(0.0));
    assert_eq!(state.previous_current_time(), TimeValue::resolved(0.0));
  }

  #[test]
  fn finished_state_did_seek_true_keeps_unconstrained_time_beyond_effect_end() {
    let mut state = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      playback_rate: 1.0,
      ..AnimationTimingState::new()
    };
    state.set_timeline_time(TimeValue::resolved(12.0));

    state.update_finished_state(
      /* associated_effect_end */ 10.0, /* did_seek */ true,
    );

    assert_eq!(state.hold_time(), TimeValue::resolved(12.0));
    assert_eq!(state.previous_current_time(), TimeValue::resolved(12.0));
  }

  #[test]
  fn finished_state_uses_previous_current_time_when_crossing_boundary_without_seeking() {
    let mut positive = AnimationTimingState {
      start_time: TimeValue::resolved(0.0),
      playback_rate: 1.0,
      previous_current_time: TimeValue::resolved(13.0),
      ..AnimationTimingState::new()
    };
    positive.set_timeline_time(TimeValue::resolved(12.0));

    positive.update_finished_state(
      /* associated_effect_end */ 10.0, /* did_seek */ false,
    );
    assert_eq!(positive.hold_time(), TimeValue::resolved(13.0));

    let mut negative = AnimationTimingState {
      start_time: TimeValue::resolved(10.0),
      playback_rate: -1.0,
      previous_current_time: TimeValue::resolved(-5.0),
      ..AnimationTimingState::new()
    };
    negative.set_timeline_time(TimeValue::resolved(12.0));

    negative.update_finished_state(
      /* associated_effect_end */ 10.0, /* did_seek */ false,
    );
    assert_eq!(negative.hold_time(), TimeValue::resolved(-5.0));
  }
}
