use rustc_hash::FxHashMap;

use super::timing::{AnimationTimingState, TimeValue};
use crate::style::types::AnimationPlayState;

#[derive(Debug, Clone)]
struct AnimationStateEntry {
  timing: AnimationTimingState,
  last_seen_frame: u64,
}

/// Persistent storage for per-animation timing state across frame renders.
///
/// Entries are keyed by `(box_id, animation_index, animation_name)` and automatically garbage
/// collected once per traversal (frame) using a monotonic frame counter.
#[derive(Debug, Default)]
pub struct AnimationStateStore {
  frame_id: u64,
  // Outer key: (box_id, animation_index).
  // Inner key: animation name. This avoids allocating a new `String` just to look up an existing
  // entry because `FxHashMap<String, _>` supports lookup by `&str`.
  entries: FxHashMap<(usize, usize), FxHashMap<String, AnimationStateEntry>>,
}

impl AnimationStateStore {
  pub fn new() -> Self {
    Self::default()
  }

  /// Marks the beginning of a new traversal frame.
  pub fn begin_frame(&mut self) {
    self.frame_id = self.frame_id.wrapping_add(1);
    if self.frame_id == 0 {
      // Avoid accidentally matching the default `last_seen_frame` value when wrapping.
      self.frame_id = 1;
    }
  }

  /// Removes any entries not observed since the most recent `begin_frame()` call.
  pub fn sweep(&mut self) {
    let frame_id = self.frame_id;
    self.entries.retain(|_, by_name| {
      by_name.retain(|_, entry| entry.last_seen_frame == frame_id);
      !by_name.is_empty()
    });
  }

  /// Returns the Web Animations `currentTime` (milliseconds) for a time-based CSS animation, while
  /// updating pause/resume bookkeeping based on `animation-play-state`.
  pub fn sample_time_based_animation(
    &mut self,
    box_id: usize,
    animation_index: usize,
    animation_name: &str,
    timeline_time_ms: f32,
    play_state: AnimationPlayState,
  ) -> f32 {
    let frame_id = self.frame_id;
    let by_name = self.entries.entry((box_id, animation_index)).or_default();
    let timeline_time = TimeValue::resolved(timeline_time_ms as f64);
    let entry = by_name
      .entry(animation_name.to_owned())
      .or_insert_with(|| {
        let mut timing = AnimationTimingState::new();
        // Initialize so that `currentTime` is 0 at the first time we sample this animation.
        timing.play(timeline_time);
        AnimationStateEntry {
          timing,
          last_seen_frame: frame_id,
        }
      });
    entry.last_seen_frame = frame_id;

    match play_state {
      AnimationPlayState::Paused => {
        // Only capture the hold time the first time we see the animation paused.
        if entry.timing.hold_time().is_unresolved() {
          entry.timing.pause(timeline_time);
        }
      }
      AnimationPlayState::Running => {
        entry.timing.play(timeline_time);
      }
    }

    entry
      .timing
      .current_time_at_timeline_time(timeline_time)
      .as_millis()
      .unwrap_or(0.0) as f32
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn time_based_animation_pauses_and_resumes_without_time_jumps() {
    let mut store = AnimationStateStore::new();

    // First sample initializes the animation such that `currentTime` is 0 at the provided timeline
    // time.
    store.begin_frame();
    let t0 = store.sample_time_based_animation(
      1,
      0,
      "fade",
      0.0,
      AnimationPlayState::Running,
    );
    assert_eq!(t0, 0.0);

    // While running, `currentTime` tracks the timeline.
    store.begin_frame();
    let t50 = store.sample_time_based_animation(
      1,
      0,
      "fade",
      50.0,
      AnimationPlayState::Running,
    );
    assert_eq!(t50, 50.0);

    // Pausing captures the current time once and freezes it.
    store.begin_frame();
    let t60_pause = store.sample_time_based_animation(
      1,
      0,
      "fade",
      60.0,
      AnimationPlayState::Paused,
    );
    assert_eq!(t60_pause, 60.0);

    store.begin_frame();
    let t100_paused = store.sample_time_based_animation(
      1,
      0,
      "fade",
      100.0,
      AnimationPlayState::Paused,
    );
    assert_eq!(t100_paused, 60.0);

    // Resuming preserves `currentTime` at the moment of resumption and continues advancing from
    // that point.
    store.begin_frame();
    let t120_resume = store.sample_time_based_animation(
      1,
      0,
      "fade",
      120.0,
      AnimationPlayState::Running,
    );
    assert_eq!(t120_resume, 60.0);

    store.begin_frame();
    let t150 = store.sample_time_based_animation(
      1,
      0,
      "fade",
      150.0,
      AnimationPlayState::Running,
    );
    assert_eq!(t150, 90.0);
  }
}
