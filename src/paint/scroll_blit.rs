//! Scroll-blit compatibility checks.
//!
//! Scroll blitting (copying pixels from the previous frame and repainting only the exposed strip)
//! is only correct when scrolling is a pure translation of the rendered output.
//!
//! CSS scroll/view timelines (and named timelines, conservatively) can make scroll affect
//! properties like opacity/transform independently of translation. When such timelines are
//! present, scroll-blit optimizations must be disabled and the renderer must fall back to a full
//! repaint.

use crate::style::ComputedStyle;
use crate::style::types::AnimationTimeline;

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
  let timelines_len = timelines.len();
  let names_len = names.len();

  if timelines_len == 0 {
    // `animation-timeline` defaults to `auto`, which is time-based (not scroll-linked).
    return false;
  }

  // Match CSS list semantics: the effective animation entry count is the max of list lengths,
  // with shorter lists repeating from the start.
  let entry_count = names_len.max(timelines_len);

  for idx in 0..entry_count {
    let name = names
      .get(idx % names_len)
      .and_then(|name| name.as_deref())
      .unwrap_or("");
    if name.is_empty() {
      continue;
    }

    if animation_timeline_is_scroll_linked(&timelines[idx % timelines_len]) {
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
  fn animation_timeline_list_repeats_to_match_names() {
    let style = ComputedStyle {
      animation_names: vec![Some("a".into()), Some("b".into())],
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    };
    assert!(
      style_uses_scroll_linked_timelines(&style),
      "expected repeated animation-timeline entries to be treated as scroll-linked"
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
}
