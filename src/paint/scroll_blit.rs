//! Scroll-blit compatibility checks.
//!
//! Scroll blitting (copying pixels from the previous frame and repainting only the exposed strip)
//! is only correct when scrolling is a pure translation of the rendered output.
//!
//! CSS scroll/view timelines (and named timelines, conservatively) can make scroll affect
//! properties like opacity/transform independently of translation. When such timelines are
//! present, scroll-blit optimizations must be disabled and the renderer must fall back to a full
//! repaint.

use crate::style::types::AnimationTimeline;
use crate::style::ComputedStyle;

pub(crate) fn style_uses_scroll_linked_timelines(style: &ComputedStyle) -> bool {
  if style_uses_scroll_linked_animation_timeline(style) {
    return true;
  }
  if let Some(backdrop) = style.backdrop.as_deref() {
    if style_uses_scroll_linked_timelines(backdrop) {
      return true;
    }
  }
  false
}

fn style_uses_scroll_linked_animation_timeline(style: &ComputedStyle) -> bool {
  let names = &style.animation_names;
  if names.is_empty() {
    return false;
  }

  let timelines = &style.animation_timelines;
  if timelines.is_empty() {
    // `animation-timeline` defaults to `auto`, which is time-based (not scroll-linked).
    return false;
  }

  // Match the engine's animation list semantics: the number of animations is defined by
  // `animation-name`, and other `animation-*` lists are indexed by the same `idx`, falling back to
  // their last value when shorter (see `animation::pick`).
  let Some(last_timeline) = timelines.last() else {
    // Defensive: `timelines.is_empty()` is handled above, but avoid panicking if the style is
    // malformed.
    return false;
  };
  for (idx, name) in names.iter().enumerate() {
    let Some(name) = name.as_deref() else {
      continue;
    };
    if name.is_empty() {
      continue;
    }

    let timeline = timelines.get(idx).unwrap_or(last_timeline);
    if animation_timeline_is_scroll_linked(timeline) {
      return true;
    }
  }

  false
}

fn animation_timeline_is_scroll_linked(timeline: &AnimationTimeline) -> bool {
  match timeline {
    AnimationTimeline::Auto | AnimationTimeline::None => false,
    // Be conservative: named timelines might refer to scroll/view timelines defined elsewhere.
    AnimationTimeline::Named(_) | AnimationTimeline::Scroll(_) | AnimationTimeline::View(_) => true,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scroll_timeline_marks_style_as_scroll_linked() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected scroll() timeline to be treated as scroll-linked"
    );
  }

  #[test]
  fn named_timeline_marks_style_as_scroll_linked() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::Named("foo".into())],
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected named timelines to be treated as scroll-linked"
    );
  }

  #[test]
  fn view_timeline_marks_style_as_scroll_linked() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::View(Default::default())],
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected view() timeline to be treated as scroll-linked"
    );
  }

  #[test]
  fn animation_timeline_list_falls_back_to_last_value_to_match_names() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into()), Some("b".into())],
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected last animation-timeline value to be treated as the fallback for subsequent animations"
    );
  }

  #[test]
  fn empty_animation_timeline_list_defaults_to_auto() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: Vec::new(),
      ..ComputedStyle::default()
    };
    assert!(
      !style_uses_scroll_linked_timelines(&style),
      "expected default/empty animation-timeline list to be treated as auto (not scroll-linked)"
    );
  }

  #[test]
  fn animation_timeline_none_is_not_scroll_linked() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::None],
      ..ComputedStyle::default()
    };
    assert!(
      !style_uses_scroll_linked_timelines(&style),
      "expected AnimationTimeline::None to be treated as not scroll-linked"
    );
  }

  #[test]
  fn empty_animation_name_list_is_not_scroll_linked_even_with_scroll_timeline_value() {
    let style = ComputedStyle {
      animation_names: Vec::new(),
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    };
    assert!(
      !style_uses_scroll_linked_timelines(&style),
      "expected no animations (empty animation-name list) to be treated as not scroll-linked"
    );
  }

  #[test]
  fn extra_animation_timeline_entries_are_ignored_when_no_corresponding_animation_names() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![
        AnimationTimeline::Auto,
        AnimationTimeline::Scroll(Default::default()),
      ],
      ..ComputedStyle::default()
    };
    assert!(
      !style_uses_scroll_linked_timelines(&style),
      "expected extra animation-timeline values beyond animation-name list to be ignored"
    );
  }

  #[test]
  fn scroll_linked_timelines_in_backdrop_are_detected() {
    let backdrop = ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::View(Default::default())],
      ..ComputedStyle::default()
    };
    let style = ComputedStyle {
      backdrop: Some(std::sync::Arc::new(backdrop)),
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected scroll-linked timelines in the backdrop pseudo-element to disable scroll blit"
    );
  }
}
