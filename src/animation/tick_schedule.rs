use std::time::Duration;

use crate::style::properties::ANIMATION_DURATION_AUTO_SENTINEL_MS;
use crate::style::types::{AnimationIterationCount, AnimationPlayState, AnimationTimeline};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

use super::AnimationStateStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnimationTickSchedule {
  /// True when there exists at least one *time-based* (`animation-timeline: auto`) animation whose
  /// sampled output can still change as document time advances.
  pub has_active_time_based_animations: bool,
  /// Smallest amount of document time until the next active finite time-based animation reaches its
  /// end.
  ///
  /// This is intended as a hint for tick schedulers that want to stop ticking promptly once the
  /// last finite animation completes.
  pub next_deadline: Option<Duration>,
}

fn pick<'a, T: Clone>(list: &'a [T], idx: usize, default: T) -> T {
  if list.is_empty() {
    return default;
  }
  list
    .get(idx)
    .cloned()
    .unwrap_or_else(|| list.last().cloned().unwrap_or(default))
}

fn duration_from_positive_ms(value_ms: f32) -> Option<Duration> {
  if !(value_ms.is_finite() && value_ms > 0.0) {
    return None;
  }
  let max_nanos = Duration::MAX.as_nanos();
  let nanos = (value_ms as f64 * 1_000_000.0).round();
  if !nanos.is_finite() {
    return None;
  }
  let nanos = (nanos as u128).min(max_nanos);
  let secs = (nanos / 1_000_000_000) as u64;
  let subsec_nanos = (nanos % 1_000_000_000) as u32;
  Some(Duration::new(secs, subsec_nanos))
}

/// Compute whether the supplied prepared fragment tree contains any *active* time-based CSS
/// animations that require periodic ticking.
///
/// This intentionally ignores scroll/view timeline animations (`animation-timeline: scroll(...)`,
/// `view(...)`, or named scroll/view timelines) because their output only changes when scroll
/// changes, which already triggers repaints.
pub fn compute_animation_tick_schedule(
  tree: &FragmentTree,
  timeline_time_ms: f32,
  mut state_store: Option<&mut AnimationStateStore>,
) -> AnimationTickSchedule {
  if tree.keyframes.is_empty() {
    return AnimationTickSchedule::default();
  }

  // Clamp non-finite or negative time inputs to 0ms so the rest of the sampling logic stays safe.
  let timeline_time_ms = if timeline_time_ms.is_finite() && timeline_time_ms >= 0.0 {
    timeline_time_ms
  } else {
    0.0
  };

  let mut has_active = false;
  // Store the smallest remaining time (delta from `timeline_time_ms`) for any active finite
  // animation.
  let mut next_deadline_ms: Option<f32> = None;

  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(&tree.root);
  for root in &tree.additional_fragments {
    stack.push(root);
  }

  while let Some(node) = stack.pop() {
    let box_id = node.box_id();
    if let Some(style) = node.style.as_deref() {
      let names = &style.animation_names;
      for idx in 0..names.len() {
        let Some(name) = names[idx].as_deref() else {
          continue;
        };

        // Skip animations that cannot resolve a `@keyframes` rule. This mirrors the apply logic in
        // `animation::apply_animations_to_node_scoped`.
        if !tree.keyframes.contains_key(name) {
          continue;
        }

        // Only tick time-based animations (`animation-timeline: auto`).
        let timeline = pick(&style.animation_timelines, idx, AnimationTimeline::Auto);
        if !matches!(timeline, AnimationTimeline::Auto) {
          continue;
        }

        let play_state = pick(
          style.animation_play_states.as_ref(),
          idx,
          AnimationPlayState::Running,
        );
        if matches!(play_state, AnimationPlayState::Paused) {
          continue;
        }

        let iteration_count = pick(
          style.animation_iteration_counts.as_ref(),
          idx,
          AnimationIterationCount::default(),
        );
        let iterations = iteration_count.as_f32();
        if !iterations.is_finite() {
          // Infinite running time-based animation => always needs ticks.
          return AnimationTickSchedule {
            has_active_time_based_animations: true,
            next_deadline: None,
          };
        }

        let raw_duration = pick(style.animation_durations.as_ref(), idx, 0.0);
        // For time-based animations, `animation-duration: auto` has no intrinsic duration and is
        // treated like `0ms` (matches `time_based_animation_state_at_current_time`).
        let duration = if raw_duration <= ANIMATION_DURATION_AUTO_SENTINEL_MS {
          0.0
        } else {
          raw_duration.max(0.0)
        };
        let delay = pick(style.animation_delays.as_ref(), idx, 0.0);

        // The Web Animations local end time in milliseconds.
        let local_end_ms = delay + duration * iterations;
        if !local_end_ms.is_finite() {
          // Be conservative when math overflows/NaNs.
          return AnimationTickSchedule {
            has_active_time_based_animations: true,
            next_deadline: None,
          };
        }

        let current_time_ms = if let (Some(store), Some(box_id)) = (state_store.as_deref_mut(), box_id)
        {
          store.sample_time_based_animation(box_id, idx, name, timeline_time_ms, play_state)
        } else {
          timeline_time_ms
        };

        if !current_time_ms.is_finite() {
          return AnimationTickSchedule {
            has_active_time_based_animations: true,
            next_deadline: None,
          };
        }

        if current_time_ms < local_end_ms {
          has_active = true;

          // Convert the local end time back into timeline coordinates using:
          //   start_time = timeline_time - current_time
          //   end_timeline = start_time + local_end
          let end_timeline_ms = timeline_time_ms - current_time_ms + local_end_ms;
          let remaining_ms = end_timeline_ms - timeline_time_ms;
          if remaining_ms.is_finite() && remaining_ms > 0.0 {
            next_deadline_ms = Some(match next_deadline_ms {
              Some(existing) => existing.min(remaining_ms),
              None => remaining_ms,
            });
          }
        }
      }
    }

    for child in node.children.iter() {
      stack.push(child);
    }
    match &node.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        stack.push(snapshot.as_ref());
      }
      _ => {}
    }
  }

  AnimationTickSchedule {
    has_active_time_based_animations: has_active,
    next_deadline: next_deadline_ms.and_then(duration_from_positive_ms),
  }
}

