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
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

/// Returns `true` when scroll-blit optimizations are safe to attempt for this fragment tree.
///
/// This currently rejects any subtree that participates in scroll-driven animations (scroll/view
/// timelines) because scroll affects visual output beyond translation.
pub(crate) fn scroll_blit_supported(tree: &FragmentTree) -> bool {
  for root in std::iter::once(&tree.root).chain(tree.additional_fragments.iter()) {
    if fragment_subtree_uses_scroll_linked_timelines(root) {
      return false;
    }
  }
  true
}

fn fragment_subtree_uses_scroll_linked_timelines(root: &FragmentNode) -> bool {
  let mut stack: Vec<&FragmentNode> = vec![root];
  while let Some(node) = stack.pop() {
    if let Some(style) = node.style.as_deref() {
      if style_uses_scroll_linked_timelines(style) {
        return true;
      }
    }
    if let Some(style) = node.starting_style.as_deref() {
      if style_uses_scroll_linked_timelines(style) {
        return true;
      }
    }
    match &node.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        stack.push(snapshot);
      }
      _ => {}
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  false
}

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
  use crate::geometry::Rect;
  use std::sync::Arc;

  #[test]
  fn scroll_timeline_disables_scroll_blit() {
    let style = Arc::new(ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    });
    let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
    let tree = FragmentTree::new(root);
    assert!(
      !scroll_blit_supported(&tree),
      "expected scroll() timeline to disable scroll blit"
    );
  }

  #[test]
  fn named_timeline_disables_scroll_blit() {
    let style = Arc::new(ComputedStyle {
      animation_names: vec![Some("a".into())],
      animation_timelines: vec![AnimationTimeline::Named("foo".into())],
      ..ComputedStyle::default()
    });
    let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
    let tree = FragmentTree::new(root);
    assert!(
      !scroll_blit_supported(&tree),
      "expected named timelines to conservatively disable scroll blit"
    );
  }

  #[test]
  fn animation_timeline_list_repeats_to_match_names() {
    let style = Arc::new(ComputedStyle {
      animation_names: vec![Some("a".into()), Some("b".into())],
      animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
      ..ComputedStyle::default()
    });
    let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
    let tree = FragmentTree::new(root);
    assert!(
      !scroll_blit_supported(&tree),
      "expected repeated animation-timeline entries to disable scroll blit"
    );
  }
}
