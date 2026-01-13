use crate::css::types::Transform;
use crate::geometry::{Rect, Size};
use crate::style::position::Position;
use crate::style::types::AnimationTimeline;
use crate::style::values::Length;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use crate::ComputedStyle;
use std::sync::Arc;

#[test]
fn viewport_fixed_element_disables_scroll_blit() {
  let mut style = ComputedStyle::default();
  style.position = Position::Fixed;
  let style = Arc::new(style);

  let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
  let tree = FragmentTree::with_viewport(root, Size::new(10.0, 10.0));

  assert!(
    !crate::scroll::scroll_blit_supported(&tree),
    "expected viewport-fixed element to disable scroll blit"
  );
}

#[test]
fn fixed_inside_fixed_containing_block_is_supported() {
  // Ancestor establishes a fixed containing block via a non-empty transform list.
  let mut ancestor_style = ComputedStyle::default();
  ancestor_style.transform.push(Transform::Translate(
    Length::px(1.0),
    Length::px(1.0),
  ));
  let ancestor_style = Arc::new(ancestor_style);

  let mut fixed_style = ComputedStyle::default();
  fixed_style.position = Position::Fixed;
  let fixed_style = Arc::new(fixed_style);
  let fixed = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![], fixed_style);

  // Include an intermediate style-less fragment so the fixed-containing-block flag must propagate
  // through nodes that don't carry computed style (e.g. synthetic fragments).
  let intermediate =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![fixed]);

  let ancestor = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![intermediate],
    ancestor_style,
  );
  let tree = FragmentTree::with_viewport(ancestor, Size::new(10.0, 10.0));

  assert!(
    crate::scroll::scroll_blit_supported(&tree),
    "expected fixed element inside fixed-containing-block ancestor to allow scroll blit"
  );
}

#[test]
fn sticky_element_disables_scroll_blit() {
  let mut root_style = ComputedStyle::default();
  let root_style = Arc::new(root_style);

  let mut sticky_style = ComputedStyle::default();
  sticky_style.position = Position::Sticky;
  let sticky_style = Arc::new(sticky_style);

  let sticky = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![], sticky_style);
  let root =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![sticky], root_style);
  let tree = FragmentTree::with_viewport(root, Size::new(10.0, 10.0));

  assert!(
    !crate::scroll::scroll_blit_supported(&tree),
    "expected sticky element to disable scroll blit"
  );
}

#[test]
fn scroll_timeline_animation_disables_scroll_blit() {
  let style = Arc::new(ComputedStyle {
    animation_names: vec![Some("a".into())],
    animation_timelines: vec![AnimationTimeline::Scroll(Default::default())],
    ..ComputedStyle::default()
  });

  let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
  let tree = FragmentTree::with_viewport(root, Size::new(10.0, 10.0));

  assert!(
    !crate::scroll::scroll_blit_supported(&tree),
    "expected scroll() animation timeline to disable scroll blit"
  );
}

#[test]
fn named_timeline_animation_disables_scroll_blit() {
  let style = Arc::new(ComputedStyle {
    animation_names: vec![Some("a".into())],
    animation_timelines: vec![AnimationTimeline::Named("foo".into())],
    ..ComputedStyle::default()
  });

  let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
  let tree = FragmentTree::with_viewport(root, Size::new(10.0, 10.0));

  assert!(
    !crate::scroll::scroll_blit_supported(&tree),
    "expected named animation timelines to conservatively disable scroll blit"
  );
}

#[test]
fn view_timeline_animation_disables_scroll_blit() {
  let style = Arc::new(ComputedStyle {
    animation_names: vec![Some("a".into())],
    animation_timelines: vec![AnimationTimeline::View(Default::default())],
    ..ComputedStyle::default()
  });

  let root = FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], style);
  let tree = FragmentTree::with_viewport(root, Size::new(10.0, 10.0));

  assert!(
    !crate::scroll::scroll_blit_supported(&tree),
    "expected view() animation timeline to disable scroll blit"
  );
}
