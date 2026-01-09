use std::sync::Arc;

use fastrender::interaction::scroll_wheel::{
  apply_wheel_scroll, apply_wheel_scroll_at_point, ScrollWheelInput,
};
use fastrender::scroll::ScrollState;
use fastrender::style::types::{Overflow, OverscrollBehavior};
use fastrender::style::ComputedStyle;
use fastrender::{FragmentContent, FragmentNode, FragmentTree, Point, Rect, Size};

fn scroll_y_style(overscroll: OverscrollBehavior) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Scroll;
  style.overscroll_behavior_y = overscroll;
  Arc::new(style)
}

fn block_with_id(
  id: usize,
  bounds: Rect,
  children: Vec<FragmentNode>,
  style: Arc<ComputedStyle>,
) -> FragmentNode {
  FragmentNode::new_with_style(
    bounds,
    FragmentContent::Block { box_id: Some(id) },
    children,
    style,
  )
}

#[test]
fn wheel_scroll_chains_inner_to_outer_to_viewport() {
  let inner_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
  let inner = block_with_id(
    2,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![inner_content],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let outer = block_with_id(
    1,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    // Ensure the outer scroller itself has scrollable overflow independent of the inner scroll
    // container (nested scrollers clip their own overflow and should not inflate ancestor scroll
    // ranges).
    vec![
      inner,
      FragmentNode::new_block(Rect::from_xywh(0.0, 250.0, 100.0, 50.0), vec![]),
    ],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  // Give the viewport a larger scrollable area so leftover delta can propagate all the way out.
  let tail = FragmentNode::new_block(Rect::from_xywh(0.0, 400.0, 100.0, 100.0), vec![]);
  let root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![outer, tail]);
  let fragment_tree = FragmentTree::new(root);

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &ScrollState::default(),
    Size::new(100.0, 100.0),
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 700.0,
    },
  );

  assert_eq!(next.element_offset(2), Point::new(0.0, 200.0), "inner clamps first");
  assert_eq!(
    next.element_offset(1),
    Point::new(0.0, 200.0),
    "outer receives leftover"
  );
  assert_eq!(
    next.viewport,
    Point::new(0.0, 300.0),
    "viewport receives remaining delta"
  );
}

#[test]
fn wheel_scroll_hit_test_accounts_for_element_scroll_offsets() {
  // Outer scroll container (already scrolled) contains an inner scroll container positioned below
  // the fold. Hit testing must apply the outer scroll offset so the wheel event targets the inner.
  let inner_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
  let inner = block_with_id(
    2,
    Rect::from_xywh(0.0, 150.0, 100.0, 100.0),
    vec![inner_content],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let spacer = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 150.0), vec![]);
  let outer = block_with_id(
    1,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![spacer, inner],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![outer]);
  let fragment_tree = FragmentTree::new(root);

  let mut scroll_state = ScrollState::default();
  scroll_state.elements.insert(1, Point::new(0.0, 100.0));

  // The inner container starts at y=150, but the outer container is already scrolled by 100px, so
  // it appears at y=50 in page coordinates.
  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Size::new(100.0, 100.0),
    Point::new(50.0, 60.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(
    next.element_offset(2),
    Point::new(0.0, 50.0),
    "wheel should scroll the visually-hit inner scroller"
  );
  assert_eq!(
    next.element_offset(1),
    Point::new(0.0, 100.0),
    "outer scroller should remain unchanged"
  );
  assert_eq!(next.viewport, Point::ZERO);
}

#[test]
fn wheel_scroll_does_not_chain_past_overscroll_none() {
  // Scroll container at its boundary with overscroll-behavior:none should not chain wheel scroll to
  // the viewport.
  let inner_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 200.0), vec![]);
  let inner = block_with_id(
    1,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![inner_content],
    scroll_y_style(OverscrollBehavior::None),
  );

  let tail = FragmentNode::new_block(Rect::from_xywh(0.0, 300.0, 100.0, 100.0), vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![inner, tail]);
  let fragment_tree = FragmentTree::new(root);

  let mut scroll_state = ScrollState::default();
  scroll_state.elements.insert(1, Point::new(0.0, 100.0));

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Size::new(100.0, 100.0),
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(
    next.element_offset(1),
    Point::new(0.0, 100.0),
    "element scroll offset remains clamped at max"
  );
  assert_eq!(next.viewport, Point::ZERO, "viewport should not receive delta");
}

#[test]
fn wheel_scroll_falls_back_to_viewport_when_hit_test_misses() {
  let tail = FragmentNode::new_block(Rect::from_xywh(0.0, 300.0, 100.0, 100.0), vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![tail]);
  let fragment_tree = FragmentTree::new(root);

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &ScrollState::default(),
    Size::new(100.0, 100.0),
    Point::new(500.0, 500.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(next.viewport, Point::new(0.0, 50.0));
}

#[test]
fn wheel_scroll_entrypoint_uses_fragment_tree_viewport_size() {
  let tail = FragmentNode::new_block(Rect::from_xywh(0.0, 400.0, 100.0, 100.0), vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![tail]);
  let fragment_tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));

  let next = apply_wheel_scroll(
    &fragment_tree,
    &ScrollState::default(),
    Point::new(500.0, 500.0),
    Point::new(0.0, 350.0),
  );

  assert_eq!(next.viewport, Point::new(0.0, 350.0));
}

#[test]
fn wheel_scroll_handles_additional_fragment_roots_without_promoting_to_viewport() {
  // Document root sits below a fixed header represented as an additional fragment root.
  let doc_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 400.0), vec![]);
  let doc_root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 100.0, 100.0, 100.0),
    vec![doc_content],
  );

  let header_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
  let header_scroller = block_with_id(
    2,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![header_content],
    scroll_y_style(OverscrollBehavior::Auto),
  );
  let header_root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![header_scroller]);

  let mut fragment_tree = FragmentTree::new(doc_root);
  fragment_tree.additional_fragments.push(header_root);

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &ScrollState::default(),
    Size::new(100.0, 100.0),
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 400.0,
    },
  );

  assert_eq!(
    next.element_offset(2),
    Point::new(0.0, 200.0),
    "scrollable content inside additional root consumes delta first"
  );
  assert_eq!(
    next.viewport,
    Point::new(0.0, 200.0),
    "leftover delta scrolls the document root viewport"
  );
}

#[test]
fn wheel_scroll_chains_to_parent_when_inner_at_limit() {
  let inner_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
  let inner = block_with_id(
    2,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![inner_content],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let outer = block_with_id(
    1,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![
      inner,
      // Ensure the outer scroller has its own scrollable overflow; the inner scroll container's
      // overflow is clipped and should not inflate ancestor scroll ranges.
      FragmentNode::new_block(Rect::from_xywh(0.0, 150.0, 100.0, 50.0), vec![]),
    ],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![outer]);
  let fragment_tree = FragmentTree::new(root);

  let mut scroll_state = ScrollState::default();
  // inner max_scroll_y = 300 - 100 = 200
  scroll_state.elements.insert(2, Point::new(0.0, 200.0));

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Size::new(100.0, 100.0),
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(next.element_offset(2), Point::new(0.0, 200.0));
  assert_eq!(next.element_offset(1), Point::new(0.0, 50.0));
  assert_eq!(next.viewport, Point::ZERO);
}

#[test]
fn wheel_scroll_respects_overscroll_contain() {
  let inner_content = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 300.0), vec![]);
  let inner = block_with_id(
    2,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![inner_content],
    scroll_y_style(OverscrollBehavior::Contain),
  );

  let outer = block_with_id(
    1,
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![inner],
    scroll_y_style(OverscrollBehavior::Auto),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![outer]);
  let fragment_tree = FragmentTree::new(root);

  let mut scroll_state = ScrollState::default();
  scroll_state.elements.insert(2, Point::new(0.0, 200.0));

  // Match `apply_wheel_scroll_at_point`'s scroll-aware hit-testing path selection.
  let mut scrolled_tree = fragment_tree.clone();
  fastrender::scroll::apply_scroll_offsets(&mut scrolled_tree, &scroll_state);
  let (root_kind, path) = scrolled_tree
    .hit_test_path(Point::new(50.0, 50.0))
    .expect("hit path on scrolled tree");
  assert_eq!(root_kind, fastrender::tree::fragment_tree::HitTestRoot::Root);
  let chain = fastrender::scroll::build_scroll_chain(&fragment_tree.root, Size::new(100.0, 100.0), &path);
  assert!(
    chain
      .iter()
      .any(|state| state.container.box_id() == Some(2) && state.overscroll_behavior_y == OverscrollBehavior::Contain),
    "expected inner scroll chain state to carry overscroll-behavior:contain"
  );

  // Ensure the scroll chain logic itself blocks propagation before calling the wheel helper.
  let mut chain = chain;
  let chain_len = chain.len();
  for (idx, state) in chain.iter_mut().enumerate() {
    if idx == chain_len - 1 {
      state.scroll = scroll_state.viewport;
    } else if let Some(id) = state.container.box_id() {
      state.scroll = scroll_state.element_offset(id);
    }
  }
  let result = fastrender::scroll::apply_scroll_chain(
    &mut chain,
    Point::new(0.0, 50.0),
    fastrender::scroll::ScrollOptions {
      source: fastrender::scroll::ScrollSource::User,
      simulate_overscroll: false,
    },
  );
  let inner_scroll = chain
    .iter()
    .find(|state| state.container.box_id() == Some(2))
    .expect("inner scroll state")
    .scroll;
  let outer_scroll = chain
    .iter()
    .find(|state| state.container.box_id() == Some(1))
    .expect("outer scroll state")
    .scroll;
  assert_eq!(inner_scroll, Point::new(0.0, 200.0));
  assert_eq!(
    outer_scroll,
    Point::ZERO,
    "overscroll-behavior:contain should stop propagation to ancestors"
  );
  assert_eq!(result.remaining, Point::ZERO);

  let next = apply_wheel_scroll_at_point(
    &fragment_tree,
    &scroll_state,
    Size::new(100.0, 100.0),
    Point::new(50.0, 50.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 50.0,
    },
  );

  assert_eq!(next.element_offset(2), Point::new(0.0, 200.0));
  assert_eq!(next.element_offset(1), Point::ZERO);
  assert_eq!(next.viewport, Point::ZERO);
}
