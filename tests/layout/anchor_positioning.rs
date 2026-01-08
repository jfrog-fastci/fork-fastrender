use std::sync::Arc;

use fastrender::css::types::Transform;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{AnchorFunction, AnchorSide, Direction, InsetValue, PositionAnchor};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::Point;
use fastrender::Rect;
use fastrender::ComputedStyle;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  fragment.iter_fragments().find(|node| match &node.content {
    FragmentContent::Block { box_id: Some(id) } => *id == box_id,
    FragmentContent::Inline { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Text { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  })
}

fn find_abs_bounds_by_box_id(fragment: &FragmentNode, box_id: usize) -> Option<Rect> {
  fn recurse(node: &FragmentNode, box_id: usize, origin: Point) -> Option<Rect> {
    let abs_bounds = node.bounds.translate(origin);
    let matches = match &node.content {
      FragmentContent::Block { box_id: Some(id) } => *id == box_id,
      FragmentContent::Inline { box_id: Some(id), .. } => *id == box_id,
      FragmentContent::Text { box_id: Some(id), .. } => *id == box_id,
      FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches {
      return Some(abs_bounds);
    }
    let child_origin = origin.translate(node.bounds.origin);
    for child in node.children_ref() {
      if let Some(found) = recurse(child, box_id, child_origin) {
        return Some(found);
      }
    }
    None
  }

  recurse(fragment, box_id, Point::ZERO)
}

#[test]
fn anchor_positioning_places_absolute_box_using_position_anchor() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 100;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (anchor_fragment.bounds.x() - 10.0).abs() < 0.1,
    "anchor should be placed at content box edge (x={})",
    anchor_fragment.bounds.x()
  );
  assert!(
    (anchor_fragment.bounds.y() - 5.0).abs() < 0.1,
    "anchor should be placed at content box edge (y={})",
    anchor_fragment.bounds.y()
  );

  assert!(
    (overlay_fragment.bounds.x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "overlay left should resolve against anchor's left edge"
  );
  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "overlay top should resolve against anchor's bottom edge"
  );
}

#[test]
fn anchor_positioning_supports_inside_outside_center_and_percentage_sides() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let outside_overlay_id = 2usize;
  let inside_overlay_id = 3usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut outside_overlay_style = ComputedStyle::default();
  outside_overlay_style.display = Display::Block;
  outside_overlay_style.position = Position::Absolute;
  outside_overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  // `outside` should resolve to the opposite side of the inset property.
  outside_overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Outside,
    fallback: None,
  });
  // `50%` should resolve to the anchor box midpoint on the relevant axis.
  outside_overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Percent(50.0),
    fallback: None,
  });
  outside_overlay_style.width = Some(Length::px(10.0));
  outside_overlay_style.height = Some(Length::px(10.0));
  outside_overlay_style.width_keyword = None;
  outside_overlay_style.height_keyword = None;
  let mut outside_overlay =
    BoxNode::new_block(Arc::new(outside_overlay_style), FormattingContextType::Block, vec![]);
  outside_overlay.id = outside_overlay_id;

  let mut inside_overlay_style = ComputedStyle::default();
  inside_overlay_style.display = Display::Block;
  inside_overlay_style.position = Position::Absolute;
  inside_overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  inside_overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Inside,
    fallback: None,
  });
  inside_overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Center,
    fallback: None,
  });
  inside_overlay_style.width = Some(Length::px(10.0));
  inside_overlay_style.height = Some(Length::px(10.0));
  inside_overlay_style.width_keyword = None;
  inside_overlay_style.height_keyword = None;
  let mut inside_overlay =
    BoxNode::new_block(Arc::new(inside_overlay_style), FormattingContextType::Block, vec![]);
  inside_overlay.id = inside_overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, outside_overlay, inside_overlay],
  );
  container.id = 104;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let outside_overlay_fragment =
    find_fragment_by_box_id(&fragment, outside_overlay_id).expect("outside overlay fragment");
  let inside_overlay_fragment =
    find_fragment_by_box_id(&fragment, inside_overlay_id).expect("inside overlay fragment");

  let expected_x = anchor_fragment.bounds.x() + anchor_fragment.bounds.width() / 2.0;
  assert!(
    (outside_overlay_fragment.bounds.x() - expected_x).abs() < 0.1,
    "percentage anchor side should resolve to the midpoint (got x={}, expected {})",
    outside_overlay_fragment.bounds.x(),
    expected_x
  );
  assert!(
    (outside_overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "outside anchor side should resolve to the opposite edge (got y={})",
    outside_overlay_fragment.bounds.y()
  );
  assert!(
    (inside_overlay_fragment.bounds.x() - expected_x).abs() < 0.1,
    "center anchor side should resolve to the midpoint (got x={}, expected {})",
    inside_overlay_fragment.bounds.x(),
    expected_x
  );
  assert!(
    (inside_overlay_fragment.bounds.y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "inside anchor side should resolve to the same edge (got y={})",
    inside_overlay_fragment.bounds.y()
  );
}

#[test]
fn anchor_positioning_uses_transformed_anchor_box() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  anchor_style.transform = vec![Transform::TranslateX(Length::px(30.0))];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 106;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.x() - (anchor_fragment.bounds.x() + 30.0)).abs() < 0.1,
    "anchor() should use the transformed anchor box (got x={}, expected {})",
    overlay_fragment.bounds.x(),
    anchor_fragment.bounds.x() + 30.0
  );
  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "transform should not affect the block axis in this test"
  );
}

#[test]
fn anchor_positioning_includes_ancestor_transforms() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let wrapper_id = 10usize;
  let anchor_id = 11usize;
  let overlay_id = 12usize;

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.transform = vec![Transform::TranslateX(Length::px(30.0))];
  wrapper_style.width = Some(Length::px(80.0));
  wrapper_style.height = Some(Length::px(40.0));
  wrapper_style.width_keyword = None;
  wrapper_style.height_keyword = None;
  let wrapper_style = Arc::new(wrapper_style);

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut wrapper = BoxNode::new_block(wrapper_style, FormattingContextType::Block, vec![anchor]);
  wrapper.id = wrapper_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![wrapper, overlay],
  );
  container.id = 107;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_bounds = find_abs_bounds_by_box_id(&fragment, anchor_id).expect("anchor bounds");
  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");

  assert!(
    (overlay_bounds.x() - (anchor_bounds.x() + 30.0)).abs() < 0.1,
    "ancestor transforms should affect the resolved anchor box (got x={}, expected {})",
    overlay_bounds.x(),
    anchor_bounds.x() + 30.0
  );
  assert!(
    (overlay_bounds.y() - anchor_bounds.max_y()).abs() < 0.1,
    "transform should not affect the block axis in this test"
  );
}

#[test]
fn anchor_positioning_resolves_with_ancestor_containing_block() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let wrapper_id = 20usize;
  let anchor_id = 21usize;
  let overlay_id = 22usize;

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.width = Some(Length::px(120.0));
  wrapper_style.height = Some(Length::px(60.0));
  wrapper_style.width_keyword = None;
  wrapper_style.height_keyword = None;
  let wrapper_style = Arc::new(wrapper_style);

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Right,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Top,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut wrapper = BoxNode::new_block(wrapper_style, FormattingContextType::Block, vec![anchor, overlay]);
  wrapper.id = wrapper_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![wrapper]);
  container.id = 108;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_bounds = find_abs_bounds_by_box_id(&fragment, anchor_id).expect("anchor bounds");
  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");

  assert!(
    (overlay_bounds.x() - anchor_bounds.max_x()).abs() < 0.1,
    "anchor() should still resolve when the containing block is an ancestor (got x={}, expected {})",
    overlay_bounds.x(),
    anchor_bounds.max_x(),
  );
  assert!(
    (overlay_bounds.y() - anchor_bounds.y()).abs() < 0.1,
    "anchor() should still resolve when the containing block is an ancestor (got y={}, expected {})",
    overlay_bounds.y(),
    anchor_bounds.y(),
  );
}

#[test]
fn anchor_positioning_supports_logical_sides() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.padding_right = Length::px(0.0);
  container_style.padding_bottom = Length::px(0.0);
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.direction = Direction::Rtl;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::InlineStart,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::BlockEnd,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![anchor, overlay]);
  container.id = 105;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.x() - anchor_fragment.bounds.max_x()).abs() < 0.1,
    "inline-start should respect the anchor's direction (RTL maps start to right) (got x={}, expected {})",
    overlay_fragment.bounds.x(),
    anchor_fragment.bounds.max_x(),
  );
  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "block-end should map to the block-end edge in the anchor's writing-mode (got y={}, expected {})",
    overlay_fragment.bounds.y(),
    anchor_fragment.bounds.max_y(),
  );
}

#[test]
fn anchor_positioning_uses_fallback_when_anchor_missing() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let overlay_id = 1usize;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--missing".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: Some(Length::px(12.0)),
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: Some(Length::px(3.0)),
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
  container.id = 101;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.x() - 3.0).abs() < 0.1,
    "missing anchor should fall back to the provided left fallback (got x={})",
    overlay_fragment.bounds.x()
  );
  assert!(
    (overlay_fragment.bounds.y() - 12.0).abs() < 0.1,
    "missing anchor should fall back to the provided top fallback (got y={})",
    overlay_fragment.bounds.y()
  );
}

#[test]
fn anchor_positioning_picks_last_anchor_in_tree_order() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(120.0));
  container_style.height = Some(Length::px(120.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let first_anchor_id = 1usize;
  let second_anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut first_anchor_style = ComputedStyle::default();
  first_anchor_style.display = Display::Block;
  first_anchor_style.width = Some(Length::px(50.0));
  first_anchor_style.height = Some(Length::px(10.0));
  first_anchor_style.width_keyword = None;
  first_anchor_style.height_keyword = None;
  first_anchor_style.anchor_names = vec!["--a".to_string()];
  let mut first_anchor =
    BoxNode::new_block(Arc::new(first_anchor_style), FormattingContextType::Block, vec![]);
  first_anchor.id = first_anchor_id;

  let mut second_anchor_style = ComputedStyle::default();
  second_anchor_style.display = Display::Block;
  second_anchor_style.width = Some(Length::px(50.0));
  second_anchor_style.height = Some(Length::px(15.0));
  second_anchor_style.width_keyword = None;
  second_anchor_style.height_keyword = None;
  second_anchor_style.anchor_names = vec!["--a".to_string()];
  let mut second_anchor =
    BoxNode::new_block(Arc::new(second_anchor_style), FormattingContextType::Block, vec![]);
  second_anchor.id = second_anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![first_anchor, second_anchor, overlay],
  );
  container.id = 102;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(120.0, 120.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let first_fragment =
    find_fragment_by_box_id(&fragment, first_anchor_id).expect("first anchor fragment");
  let second_fragment =
    find_fragment_by_box_id(&fragment, second_anchor_id).expect("second anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.y() - second_fragment.bounds.max_y()).abs() < 0.1,
    "duplicate anchor names should resolve against the last fragment in tree order"
  );
  assert!(
    (overlay_fragment.bounds.y() - first_fragment.bounds.max_y()).abs() > 0.1,
    "overlay should not resolve against the first matching anchor"
  );
}

#[test]
fn anchor_positioning_allows_explicit_anchor_name_in_anchor_function() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(120.0));
  container_style.height = Some(Length::px(120.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(40.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  // Deliberately point `position-anchor` at a missing anchor to ensure the explicit name is used.
  overlay_style.position_anchor = PositionAnchor::Name("--missing".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: Some("--a".to_string()),
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: Some("--a".to_string()),
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 103;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(120.0, 120.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "overlay should use the explicit anchor name for left resolution"
  );
  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "overlay should use the explicit anchor name for top resolution"
  );
}
