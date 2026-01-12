use std::sync::Arc;

use crate::css::types::{Declaration, PropertyValue, Transform};
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::position::Position;
use crate::style::position_try::PositionTryRegistry;
use crate::style::types::{
  AnchorFunction, AnchorScope, AnchorSide, AnchorSizeAxis, AnchorSizeFunction, Direction,
  InsetValue, PositionAnchor, PositionTryOrder, WritingMode,
};
use crate::style::values::Length;
use crate::tree::box_tree::{BoxNode, GeneratedPseudoElement};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::ComputedStyle;
use crate::Point;
use crate::Rect;

fn find_fragment_by_box_id<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
) -> Option<&'a FragmentNode> {
  fragment.iter_fragments().find(|node| match &node.content {
    FragmentContent::Block { box_id: Some(id) } => *id == box_id,
    FragmentContent::Inline {
      box_id: Some(id), ..
    } => *id == box_id,
    FragmentContent::Text {
      box_id: Some(id), ..
    } => *id == box_id,
    FragmentContent::Replaced {
      box_id: Some(id), ..
    } => *id == box_id,
    _ => false,
  })
}

fn find_abs_bounds_by_box_id(fragment: &FragmentNode, box_id: usize) -> Option<Rect> {
  fn recurse(node: &FragmentNode, box_id: usize, origin: Point) -> Option<Rect> {
    let abs_bounds = node.bounds.translate(origin);
    let matches = match &node.content {
      FragmentContent::Block { box_id: Some(id) } => *id == box_id,
      FragmentContent::Inline {
        box_id: Some(id), ..
      } => *id == box_id,
      FragmentContent::Text {
        box_id: Some(id), ..
      } => *id == box_id,
      FragmentContent::Replaced {
        box_id: Some(id), ..
      } => *id == box_id,
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

fn decl(property: &'static str, value: PropertyValue) -> Declaration {
  Declaration {
    property: property.into(),
    value,
    raw_value: String::new(),
    important: false,
    contains_var: false,
  }
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
fn anchor_positioning_uses_implicit_anchor_for_position_anchor_auto() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  let container_style = Arc::new(container_style);

  let origin_id = 1usize;
  let overlay_id = 2usize;

  let mut origin_style = ComputedStyle::default();
  origin_style.display = Display::Block;
  origin_style.width = Some(Length::px(50.0));
  origin_style.height = Some(Length::px(20.0));
  origin_style.width_keyword = None;
  origin_style.height_keyword = None;
  let mut origin = BoxNode::new_block(Arc::new(origin_style), FormattingContextType::Block, vec![]);
  origin.id = origin_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Auto;
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;
  overlay.implicit_anchor_box_id = Some(origin_id);

  origin.children.push(overlay);

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![origin]);
  container.id = 101;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let origin_bounds = find_abs_bounds_by_box_id(&fragment, origin_id).expect("origin bounds");
  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");

  assert!(
    (overlay_bounds.x() - origin_bounds.x()).abs() < 0.1,
    "overlay left should resolve against the implicit anchor's left edge (overlay x={}, origin x={})",
    overlay_bounds.x(),
    origin_bounds.x()
  );
  assert!(
    (overlay_bounds.y() - origin_bounds.max_y()).abs() < 0.1,
    "overlay top should resolve against the implicit anchor's bottom edge (overlay y={}, origin bottom={})",
    overlay_bounds.y(),
    origin_bounds.max_y()
  );
}

#[test]
fn anchor_positioning_position_anchor_auto_without_implicit_anchor_uses_fallback() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let overlay_id = 1usize;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Auto;
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Top,
    fallback: Some(Length::px(9.0)),
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: Some(Length::px(7.0)),
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;
  overlay.implicit_anchor_box_id = None;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
  container.id = 102;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");
  assert!(
    (overlay_bounds.x() - 7.0).abs() < 0.1,
    "overlay left should resolve to the anchor() fallback value"
  );
  assert!(
    (overlay_bounds.y() - 9.0).abs() < 0.1,
    "overlay top should resolve to the anchor() fallback value"
  );
}

#[test]
fn anchor_positioning_named_position_anchor_overrides_implicit_anchor() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let named_anchor_id = 1usize;
  let origin_id = 2usize;
  let overlay_id = 3usize;

  let mut named_anchor_style = ComputedStyle::default();
  named_anchor_style.display = Display::Block;
  named_anchor_style.width = Some(Length::px(30.0));
  named_anchor_style.height = Some(Length::px(10.0));
  named_anchor_style.width_keyword = None;
  named_anchor_style.height_keyword = None;
  named_anchor_style.margin_left = Some(Length::px(50.0));
  named_anchor_style.margin_bottom = Some(Length::px(20.0));
  named_anchor_style.anchor_names = vec!["--a".to_string()];
  let mut named_anchor = BoxNode::new_block(
    Arc::new(named_anchor_style),
    FormattingContextType::Block,
    vec![],
  );
  named_anchor.id = named_anchor_id;

  let mut origin_style = ComputedStyle::default();
  origin_style.display = Display::Block;
  origin_style.width = Some(Length::px(100.0));
  origin_style.height = Some(Length::px(60.0));
  origin_style.width_keyword = None;
  origin_style.height_keyword = None;
  let mut origin = BoxNode::new_block(
    Arc::new(origin_style),
    FormattingContextType::Block,
    vec![named_anchor],
  );
  origin.id = origin_id;

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
  overlay_style.width = Some(Length::px(5.0));
  overlay_style.height = Some(Length::px(5.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;
  overlay.implicit_anchor_box_id = Some(origin_id);

  origin.children.push(overlay);

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![origin]);
  container.id = 103;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let named_anchor_bounds =
    find_abs_bounds_by_box_id(&fragment, named_anchor_id).expect("named anchor bounds");
  let origin_bounds = find_abs_bounds_by_box_id(&fragment, origin_id).expect("origin bounds");
  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");

  assert!(
    (overlay_bounds.x() - named_anchor_bounds.x()).abs() < 0.1,
    "named position-anchor should take precedence over implicit anchor (overlay x={}, named x={})",
    overlay_bounds.x(),
    named_anchor_bounds.x()
  );
  assert!(
    (overlay_bounds.y() - named_anchor_bounds.max_y()).abs() < 0.1,
    "named position-anchor should take precedence over implicit anchor (overlay y={}, named bottom={})",
    overlay_bounds.y(),
    named_anchor_bounds.max_y()
  );
  assert!(
    (overlay_bounds.x() - origin_bounds.x()).abs() > 0.1
      || (overlay_bounds.y() - origin_bounds.max_y()).abs() > 0.1,
    "overlay should not resolve against the implicit anchor when a named anchor is set"
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
  let mut outside_overlay = BoxNode::new_block(
    Arc::new(outside_overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
  let mut inside_overlay = BoxNode::new_block(
    Arc::new(inside_overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
fn anchor_positioning_applies_position_try_fallbacks_to_avoid_overflow() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut position_try_registry = PositionTryRegistry::default();
  position_try_registry.register(
    "--flip".to_string(),
    vec![
      decl("left", PropertyValue::Keyword("auto".to_string())),
      decl("right", PropertyValue::Keyword("anchor(left)".to_string())),
    ],
  );
  let position_try_registry = Arc::new(position_try_registry);

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
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_registry = position_try_registry;
  overlay_style.position_try_fallbacks = vec!["--flip".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 100;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (anchor_fragment.bounds.x() - 80.0).abs() < 0.1,
    "anchor should be offset by its margin-left"
  );
  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "fallback should place overlay to the left of the anchor (got overlay max_x={}, anchor x={})",
    overlay_fragment.bounds.max_x(),
    anchor_fragment.bounds.x()
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying the fallback"
  );
}

#[test]
fn anchor_positioning_uses_first_position_try_fallback_that_fits() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut position_try_registry = PositionTryRegistry::default();
  // First try set keeps the original overflowing placement.
  position_try_registry.register(
    "--overflow".to_string(),
    vec![decl(
      "left",
      PropertyValue::Keyword("anchor(right)".to_string()),
    )],
  );
  // Second try set flips the overlay to fit inside the containing block.
  position_try_registry.register(
    "--flip".to_string(),
    vec![
      decl("left", PropertyValue::Keyword("auto".to_string())),
      decl("right", PropertyValue::Keyword("anchor(left)".to_string())),
    ],
  );
  let position_try_registry = Arc::new(position_try_registry);

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
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_registry = position_try_registry;
  overlay_style.position_try_fallbacks = vec!["--overflow".to_string(), "--flip".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 101;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "should fall back to the second try set that fits (overlay max_x={}, anchor x={})",
    overlay_fragment.bounds.max_x(),
    anchor_fragment.bounds.x()
  );
  assert!(
    overlay_fragment.bounds.x() < anchor_fragment.bounds.x(),
    "overlay should be placed to the left of the anchor after falling back"
  );
}

#[test]
fn anchor_positioning_supports_builtin_flip_inline_try() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
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
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec!["flip-inline".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 102;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "flip-inline should place overlay to the left of the anchor"
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-inline"
  );
}

#[test]
fn anchor_positioning_flip_inline_respects_writing_mode_axis() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let spacer_id = 1usize;
  let anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(80.0));
  spacer_style.height_keyword = None;
  let mut spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
  spacer.id = spacer_id;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  // In vertical writing modes the inline axis is vertical, so `flip-inline` should swap top/bottom.
  overlay_style.writing_mode = WritingMode::VerticalRl;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(30.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec!["flip-inline".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![spacer, anchor, overlay],
  );
  container.id = 204;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "flip-inline should place overlay above the anchor when the inline axis is vertical"
  );
  assert!(
    overlay_fragment.bounds.max_y() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-inline"
  );
}

#[test]
fn anchor_positioning_supports_builtin_flip_block_try() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let spacer_id = 1usize;
  let anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(80.0));
  spacer_style.height_keyword = None;
  let mut spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
  spacer.id = spacer_id;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
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
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(30.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec!["flip-block".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![spacer, anchor, overlay],
  );
  container.id = 103;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "flip-block should place overlay above the anchor"
  );
  assert!(
    overlay_fragment.bounds.max_y() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-block"
  );
}

#[test]
fn anchor_positioning_supports_builtin_flip_x_try_in_vertical_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  // In vertical writing modes the inline axis is vertical, so `flip-x` should swap left/right.
  overlay_style.writing_mode = WritingMode::VerticalRl;
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
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec!["flip-x".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 206;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "flip-x should place overlay to the left of the anchor"
  );
  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "flip-x should not affect the y-axis in this test"
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-x"
  );
}

#[test]
fn anchor_positioning_supports_builtin_flip_y_try_in_vertical_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let spacer_id = 1usize;
  let anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(80.0));
  spacer_style.height_keyword = None;
  let mut spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
  spacer.id = spacer_id;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  // In vertical writing modes the block axis is horizontal, so `flip-y` should swap top/bottom.
  overlay_style.writing_mode = WritingMode::VerticalRl;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(30.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec!["flip-y".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![spacer, anchor, overlay],
  );
  container.id = 207;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "flip-y should place overlay above the anchor"
  );
  assert!(
    overlay_fragment.bounds.max_y() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-y"
  );
}

#[test]
fn anchor_positioning_supports_builtin_flip_start_try_for_anchor_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(120.0));
  container_style.height = Some(Length::px(220.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(200.0));
  anchor_style.height = Some(Length::px(100.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  // Place at the containing block origin so overflow comes only from sizing.
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Width,
    fallback: None,
  });
  overlay_style.height = Some(Length::px(50.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  // Base styles overflow horizontally (200px wide in a 120px CB); flip-start should swap width/height
  // so the box can fit (50px wide, 100px tall).
  overlay_style.position_try_fallbacks = vec!["flip-start".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 208;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(120.0, 220.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.width() - 50.0).abs() < 0.1,
    "flip-start should swap width/height so width becomes 50px (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 100.0).abs() < 0.1,
    "flip-start should swap anchor-size(width) so height becomes 100px (got h={})",
    overlay_fragment.bounds.height()
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 120.0 + 0.1
      && overlay_fragment.bounds.max_y() <= 220.0 + 0.1,
    "overlay should not overflow the containing block after applying flip-start"
  );
}

#[test]
fn anchor_positioning_supports_multiple_builtin_try_tactics() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let spacer_id = 1usize;
  let anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(80.0));
  spacer_style.height_keyword = None;
  let mut spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
  spacer.id = spacer_id;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
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
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(30.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_fallbacks = vec![
    "flip-inline".to_string(),
    "flip-block".to_string(),
    "flip-inline flip-block".to_string(),
  ];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![spacer, anchor, overlay],
  );
  container.id = 205;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "flip-inline flip-block should place overlay to the left of the anchor"
  );
  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "flip-inline flip-block should place overlay above the anchor"
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1
      && overlay_fragment.bounds.max_y() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying the multi-tactic fallback"
  );
}

#[test]
fn anchor_positioning_sorts_position_try_fallbacks_by_most_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let overlay_id = 1usize;

  let mut position_try_registry = PositionTryRegistry::default();
  // A fallback that fits, but reduces the available width (left inset = 20px).
  position_try_registry.register(
    "--narrow".to_string(),
    vec![decl("left", PropertyValue::Length(Length::px(20.0)))],
  );
  // A fallback that fits and provides the full containing block width (left auto, right 0).
  position_try_registry.register(
    "--wide".to_string(),
    vec![
      decl("left", PropertyValue::Keyword("auto".to_string())),
      decl("right", PropertyValue::Length(Length::px(0.0))),
    ],
  );
  let position_try_registry = Arc::new(position_try_registry);

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  // Base placement overflows horizontally: x=80, width=30 → max_x=110.
  overlay_style.left = InsetValue::Length(Length::px(80.0));
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_registry = position_try_registry;
  overlay_style.position_try_fallbacks = vec!["--narrow".to_string(), "--wide".to_string()];
  overlay_style.position_try_order = PositionTryOrder::MostWidth;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
  container.id = 209;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.x() - 70.0).abs() < 0.1,
    "most-width should prefer the fallback that provides more available space (got x={})",
    overlay_fragment.bounds.x()
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying the most-width ordering"
  );
}

#[test]
fn anchor_positioning_allows_try_set_and_builtin_tactic_in_single_fallback() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let spacer_id = 1usize;
  let anchor_id = 2usize;
  let overlay_id = 3usize;

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(80.0));
  spacer_style.height_keyword = None;
  let mut spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
  spacer.id = spacer_id;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(10.0));
  anchor_style.height = Some(Length::px(10.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut position_try_registry = PositionTryRegistry::default();
  position_try_registry.register(
    "--above-right".to_string(),
    vec![
      decl("top", PropertyValue::Keyword("auto".to_string())),
      decl("bottom", PropertyValue::Keyword("anchor(top)".to_string())),
      // Explicitly reset horizontal insets so the following built-in try tactic must run after the
      // try set (otherwise it would be overwritten and the overlay would still overflow).
      decl("left", PropertyValue::Keyword("anchor(right)".to_string())),
      decl("right", PropertyValue::Keyword("auto".to_string())),
    ],
  );
  let position_try_registry = Arc::new(position_try_registry);

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  // Start in the bottom-right corner so the base placement overflows both axes.
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Right,
    fallback: None,
  });
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay_style.width = Some(Length::px(30.0));
  overlay_style.height = Some(Length::px(30.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  overlay_style.position_try_registry = position_try_registry;
  overlay_style.position_try_fallbacks = vec!["--above-right flip-inline".to_string()];
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![spacer, anchor, overlay],
  );
  container.id = 208;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "fallback should place overlay to the left of the anchor"
  );
  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "fallback should place overlay above the anchor"
  );
  assert!(
    overlay_fragment.bounds.max_x() <= 100.0 + 0.1
      && overlay_fragment.bounds.max_y() <= 100.0 + 0.1,
    "overlay should not overflow the containing block after applying the combined fallback"
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut wrapper = BoxNode::new_block(
    wrapper_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
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
  container_style.direction = Direction::Rtl;
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 105;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.x() - anchor_fragment.bounds.max_x()).abs() < 0.1,
    "inline-start should respect the containing block direction (RTL maps start to right) (got x={}, expected {})",
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
fn anchor_positioning_inline_sides_respect_rtl_when_inline_axis_is_vertical() {
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    for direction in [Direction::Ltr, Direction::Rtl] {
      let mut container_style = ComputedStyle::default();
      container_style.display = Display::Block;
      container_style.position = Position::Relative;
      container_style.writing_mode = writing_mode;
      container_style.direction = direction;
      container_style.width = Some(Length::px(200.0));
      container_style.height = Some(Length::px(200.0));
      container_style.width_keyword = None;
      container_style.height_keyword = None;
      let container_style = Arc::new(container_style);

      let anchor_id = 1usize;
      let overlay_id = 2usize;

      let mut anchor_style = ComputedStyle::default();
      anchor_style.display = Display::Block;
      anchor_style.writing_mode = writing_mode;
      anchor_style.direction = direction;
      anchor_style.width = Some(Length::px(50.0));
      anchor_style.height = Some(Length::px(20.0));
      anchor_style.width_keyword = None;
      anchor_style.height_keyword = None;
      anchor_style.anchor_names = vec!["--a".to_string()];
      let mut anchor =
        BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
      anchor.id = anchor_id;

      let mut overlay_style = ComputedStyle::default();
      overlay_style.display = Display::Block;
      overlay_style.position = Position::Absolute;
      overlay_style.writing_mode = writing_mode;
      overlay_style.direction = direction;
      overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
      overlay_style.left = InsetValue::Length(Length::px(0.0));
      overlay_style.top = InsetValue::Anchor(AnchorFunction {
        name: None,
        side: AnchorSide::InlineStart,
        fallback: None,
      });
      overlay_style.width = Some(Length::px(10.0));
      overlay_style.height = Some(Length::px(10.0));
      overlay_style.width_keyword = None;
      overlay_style.height_keyword = None;
      let mut overlay = BoxNode::new_block(
        Arc::new(overlay_style),
        FormattingContextType::Block,
        vec![],
      );
      overlay.id = overlay_id;

      let mut container = BoxNode::new_block(
        container_style,
        FormattingContextType::Block,
        vec![anchor, overlay],
      );
      container.id = 205;

      let fc = BlockFormattingContext::new();
      let constraints = LayoutConstraints::definite(200.0, 200.0);
      let fragment = fc.layout(&container, &constraints).expect("layout");

      let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
      let overlay_fragment =
        find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

      // Mirror the `inline_axis_positive` logic used by layout: in `sideways-lr` the inline axis
      // direction is flipped relative to vertical writing modes.
      let inline_positive = match writing_mode {
        WritingMode::SidewaysLr => matches!(direction, Direction::Rtl),
        _ => !matches!(direction, Direction::Rtl),
      };
      let expected_y = if inline_positive {
        anchor_fragment.bounds.y()
      } else {
        anchor_fragment.bounds.max_y()
      };

      assert!(
        (overlay_fragment.bounds.y() - expected_y).abs() < 0.1,
        "inline-start should map to the {:?} edge under {writing_mode:?} + {direction:?} (got y={}, expected {})",
        if inline_positive { "top" } else { "bottom" },
        overlay_fragment.bounds.y(),
        expected_y,
      );
    }
  }
}

#[test]
fn anchor_positioning_start_end_resolve_against_containing_block_axis() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let start_overlay_id = 2usize;
  let end_overlay_id = 3usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut start_overlay_style = ComputedStyle::default();
  start_overlay_style.display = Display::Block;
  start_overlay_style.position = Position::Absolute;
  start_overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  start_overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Start,
    fallback: None,
  });
  start_overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Start,
    fallback: None,
  });
  start_overlay_style.width = Some(Length::px(10.0));
  start_overlay_style.height = Some(Length::px(10.0));
  start_overlay_style.width_keyword = None;
  start_overlay_style.height_keyword = None;
  let mut start_overlay = BoxNode::new_block(
    Arc::new(start_overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  start_overlay.id = start_overlay_id;

  let mut end_overlay_style = ComputedStyle::default();
  end_overlay_style.display = Display::Block;
  end_overlay_style.position = Position::Absolute;
  end_overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  end_overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::End,
    fallback: None,
  });
  end_overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::End,
    fallback: None,
  });
  end_overlay_style.width = Some(Length::px(10.0));
  end_overlay_style.height = Some(Length::px(10.0));
  end_overlay_style.width_keyword = None;
  end_overlay_style.height_keyword = None;
  let mut end_overlay = BoxNode::new_block(
    Arc::new(end_overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  end_overlay.id = end_overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, start_overlay, end_overlay],
  );
  container.id = 300;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let start_fragment =
    find_fragment_by_box_id(&fragment, start_overlay_id).expect("start overlay fragment");
  let end_fragment =
    find_fragment_by_box_id(&fragment, end_overlay_id).expect("end overlay fragment");

  // Under vertical-rl, the horizontal axis corresponds to the *block* axis (block-start is on the
  // right), while the vertical axis corresponds to the inline axis (inline-start is top for LTR).
  assert!(
    (start_fragment.bounds.x() - anchor_fragment.bounds.max_x()).abs() < 0.1,
    "anchor(start) in the left inset should resolve to the block-start edge under vertical writing modes (got x={}, expected {})",
    start_fragment.bounds.x(),
    anchor_fragment.bounds.max_x(),
  );
  assert!(
    (start_fragment.bounds.y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "anchor(start) in the top inset should resolve to the inline-start edge under vertical writing modes (got y={}, expected {})",
    start_fragment.bounds.y(),
    anchor_fragment.bounds.y(),
  );

  assert!(
    (end_fragment.bounds.x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "anchor(end) in the left inset should resolve to the block-end edge under vertical writing modes (got x={}, expected {})",
    end_fragment.bounds.x(),
    anchor_fragment.bounds.x(),
  );
  assert!(
    (end_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "anchor(end) in the top inset should resolve to the inline-end edge under vertical writing modes (got y={}, expected {})",
    end_fragment.bounds.y(),
    anchor_fragment.bounds.max_y(),
  );
}

#[test]
fn anchor_positioning_self_start_end_resolve_against_positioned_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let start_overlay_id = 2usize;
  let self_start_overlay_id = 3usize;
  let self_end_overlay_id = 4usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(50.0));
  anchor_style.height = Some(Length::px(20.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  // Both overlays use a vertical writing mode so `self-start` produces a different physical edge
  // than `start`, which always resolves against the containing block's writing mode.
  let overlay_base = |side: AnchorSide| {
    let mut overlay_style = ComputedStyle::default();
    overlay_style.display = Display::Block;
    overlay_style.position = Position::Absolute;
    overlay_style.writing_mode = WritingMode::VerticalRl;
    overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
    overlay_style.left = InsetValue::Anchor(AnchorFunction {
      name: None,
      side,
      fallback: None,
    });
    overlay_style.top = InsetValue::Length(Length::px(0.0));
    overlay_style.width = Some(Length::px(10.0));
    overlay_style.height = Some(Length::px(10.0));
    overlay_style.width_keyword = None;
    overlay_style.height_keyword = None;
    BoxNode::new_block(
      Arc::new(overlay_style),
      FormattingContextType::Block,
      vec![],
    )
  };

  let mut start_overlay = overlay_base(AnchorSide::Start);
  start_overlay.id = start_overlay_id;
  let mut self_start_overlay = overlay_base(AnchorSide::SelfStart);
  self_start_overlay.id = self_start_overlay_id;
  let mut self_end_overlay = overlay_base(AnchorSide::SelfEnd);
  self_end_overlay.id = self_end_overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, start_overlay, self_start_overlay, self_end_overlay],
  );
  container.id = 301;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let start_fragment =
    find_fragment_by_box_id(&fragment, start_overlay_id).expect("start overlay fragment");
  let self_start_fragment =
    find_fragment_by_box_id(&fragment, self_start_overlay_id).expect("self-start overlay fragment");
  let self_end_fragment =
    find_fragment_by_box_id(&fragment, self_end_overlay_id).expect("self-end overlay fragment");

  assert!(
    (start_fragment.bounds.x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "anchor(start) should resolve against the containing block writing mode (got x={}, expected {})",
    start_fragment.bounds.x(),
    anchor_fragment.bounds.x(),
  );
  assert!(
    (self_start_fragment.bounds.x() - anchor_fragment.bounds.max_x()).abs() < 0.1,
    "anchor(self-start) should resolve against the positioned element writing mode (got x={}, expected {})",
    self_start_fragment.bounds.x(),
    anchor_fragment.bounds.max_x(),
  );
  assert!(
    (self_end_fragment.bounds.x() - anchor_fragment.bounds.x()).abs() < 0.1,
    "anchor(self-end) should resolve against the positioned element writing mode (got x={}, expected {})",
    self_end_fragment.bounds.x(),
    anchor_fragment.bounds.x(),
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
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
fn anchor_positioning_anchor_size_sets_absolute_box_dimensions() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
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
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Width,
    fallback: None,
  });
  overlay_style.height_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Height,
    fallback: None,
  });
  // Clamp via max-width to ensure anchor-size participates in min/max sizing.
  overlay_style.max_width = Some(Length::px(40.0));
  overlay_style.max_width_keyword = None;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 111;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.width() - 40.0).abs() < 0.1,
    "anchor-size(width) should size the overlay, then max-width should clamp it (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 20.0).abs() < 0.1,
    "anchor-size(height) should size the overlay height (got h={})",
    overlay_fragment.bounds.height()
  );
}

#[test]
fn anchor_positioning_anchor_size_inline_block_respects_containing_block_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
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
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Inline,
    fallback: None,
  });
  overlay_style.height_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Block,
    fallback: None,
  });
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 112;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.width() - 20.0).abs() < 0.1,
    "inline should map to the anchor's physical height under vertical writing modes (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 40.0).abs() < 0.1,
    "block should map to the anchor's physical width under vertical writing modes (got h={})",
    overlay_fragment.bounds.height()
  );
}

#[test]
fn anchor_positioning_anchor_size_self_axes_respect_positioned_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
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
  overlay_style.writing_mode = WritingMode::VerticalRl;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::SelfInline,
    fallback: None,
  });
  overlay_style.height_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::SelfBlock,
    fallback: None,
  });
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 113;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.width() - 20.0).abs() < 0.1,
    "self-inline should map to the anchor's physical height under vertical writing modes (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 40.0).abs() < 0.1,
    "self-block should map to the anchor's physical width under vertical writing modes (got h={})",
    overlay_fragment.bounds.height()
  );
}

#[test]
fn anchor_positioning_anchor_size_axis_omission_defaults_to_property_axis() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let anchor_id = 1usize;
  let overlay_id = 2usize;

  let mut anchor_style = ComputedStyle::default();
  anchor_style.display = Display::Block;
  anchor_style.width = Some(Length::px(33.0));
  anchor_style.height = Some(Length::px(44.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Omitted,
    fallback: None,
  });
  overlay_style.height_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Omitted,
    fallback: None,
  });
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![anchor, overlay],
  );
  container.id = 114;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.width() - 33.0).abs() < 0.1,
    "anchor-size() should default to the width axis for the width property (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 44.0).abs() < 0.1,
    "anchor-size() should default to the height axis for the height property (got h={})",
    overlay_fragment.bounds.height()
  );
}

#[test]
fn anchor_positioning_anchor_size_uses_fallback_when_anchor_missing() {
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
  overlay_style.top = InsetValue::Length(Length::px(0.0));
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Width,
    fallback: Some(Length::px(33.0)),
  });
  overlay_style.height_anchor_size = Some(AnchorSizeFunction {
    name: None,
    axis: AnchorSizeAxis::Height,
    fallback: Some(Length::px(44.0)),
  });
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
  container.id = 113;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.width() - 33.0).abs() < 0.1,
    "missing anchor should fall back to the provided width fallback (got w={})",
    overlay_fragment.bounds.width()
  );
  assert!(
    (overlay_fragment.bounds.height() - 44.0).abs() < 0.1,
    "missing anchor should fall back to the provided height fallback (got h={})",
    overlay_fragment.bounds.height()
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
  let mut first_anchor = BoxNode::new_block(
    Arc::new(first_anchor_style),
    FormattingContextType::Block,
    vec![],
  );
  first_anchor.id = first_anchor_id;

  let mut second_anchor_style = ComputedStyle::default();
  second_anchor_style.display = Display::Block;
  second_anchor_style.width = Some(Length::px(50.0));
  second_anchor_style.height = Some(Length::px(15.0));
  second_anchor_style.width_keyword = None;
  second_anchor_style.height_keyword = None;
  second_anchor_style.anchor_names = vec!["--a".to_string()];
  let mut second_anchor = BoxNode::new_block(
    Arc::new(second_anchor_style),
    FormattingContextType::Block,
    vec![],
  );
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
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

#[test]
fn anchor_positioning_respects_anchor_scope_for_nested_positioned_descendants() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let wrapper1_id = 10usize;
  let wrapper2_id = 11usize;
  let overlay1_id = 20usize;
  let overlay2_id = 21usize;

  let mut wrapper1_style = ComputedStyle::default();
  wrapper1_style.display = Display::Block;
  wrapper1_style.width = Some(Length::px(40.0));
  wrapper1_style.height = Some(Length::px(20.0));
  wrapper1_style.width_keyword = None;
  wrapper1_style.height_keyword = None;
  wrapper1_style.anchor_names = vec!["--a".to_string()];
  wrapper1_style.anchor_scope = AnchorScope::Names(vec!["--a".to_string()]);

  let mut overlay1_style = ComputedStyle::default();
  overlay1_style.display = Display::Block;
  overlay1_style.position = Position::Absolute;
  overlay1_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay1_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay1_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Right,
    fallback: None,
  });
  overlay1_style.width = Some(Length::px(10.0));
  overlay1_style.height = Some(Length::px(10.0));
  overlay1_style.width_keyword = None;
  overlay1_style.height_keyword = None;

  let mut overlay1 = BoxNode::new_block(
    Arc::new(overlay1_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay1.id = overlay1_id;
  let mut wrapper1 = BoxNode::new_block(
    Arc::new(wrapper1_style),
    FormattingContextType::Block,
    vec![overlay1],
  );
  wrapper1.id = wrapper1_id;

  let mut wrapper2_style = ComputedStyle::default();
  wrapper2_style.display = Display::Block;
  wrapper2_style.width = Some(Length::px(60.0));
  wrapper2_style.height = Some(Length::px(30.0));
  wrapper2_style.width_keyword = None;
  wrapper2_style.height_keyword = None;
  wrapper2_style.anchor_names = vec!["--a".to_string()];
  wrapper2_style.anchor_scope = AnchorScope::Names(vec!["--a".to_string()]);

  let mut overlay2_style = ComputedStyle::default();
  overlay2_style.display = Display::Block;
  overlay2_style.position = Position::Absolute;
  overlay2_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay2_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  overlay2_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Right,
    fallback: None,
  });
  overlay2_style.width = Some(Length::px(10.0));
  overlay2_style.height = Some(Length::px(10.0));
  overlay2_style.width_keyword = None;
  overlay2_style.height_keyword = None;

  let mut overlay2 = BoxNode::new_block(
    Arc::new(overlay2_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay2.id = overlay2_id;
  let mut wrapper2 = BoxNode::new_block(
    Arc::new(wrapper2_style),
    FormattingContextType::Block,
    vec![overlay2],
  );
  wrapper2.id = wrapper2_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![wrapper1, wrapper2],
  );
  container.id = 109;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let wrapper1_fragment = find_fragment_by_box_id(&fragment, wrapper1_id).expect("wrapper1");
  let wrapper2_fragment = find_fragment_by_box_id(&fragment, wrapper2_id).expect("wrapper2");
  let wrapper1_bounds = find_abs_bounds_by_box_id(&fragment, wrapper1_id).expect("wrapper1 bounds");
  let wrapper2_bounds = find_abs_bounds_by_box_id(&fragment, wrapper2_id).expect("wrapper2 bounds");
  let overlay1_bounds = find_abs_bounds_by_box_id(&fragment, overlay1_id).expect("overlay1 bounds");
  let overlay2_bounds = find_abs_bounds_by_box_id(&fragment, overlay2_id).expect("overlay2 bounds");

  let wrapper1_style = wrapper1_fragment.style.as_ref().expect("wrapper1 style");
  let wrapper2_style = wrapper2_fragment.style.as_ref().expect("wrapper2 style");
  assert_eq!(
    wrapper1_style.anchor_names,
    vec!["--a".to_string()],
    "wrapper1 should expose anchor-name"
  );
  assert_eq!(
    wrapper2_style.anchor_names,
    vec!["--a".to_string()],
    "wrapper2 should expose anchor-name"
  );
  assert!(
    matches!(wrapper1_style.anchor_scope, AnchorScope::Names(_)),
    "wrapper1 should expose anchor-scope"
  );
  assert!(
    matches!(wrapper2_style.anchor_scope, AnchorScope::Names(_)),
    "wrapper2 should expose anchor-scope"
  );

  let overlay1_expected_x = wrapper1_bounds.max_x();
  let overlay1_expected_y = wrapper1_bounds.max_y();
  let overlay2_expected_x = wrapper2_bounds.max_x();
  let overlay2_expected_y = wrapper2_bounds.max_y();

  assert!(
    (overlay1_bounds.x() - overlay1_expected_x).abs() < 0.1,
    "overlay1 should resolve against wrapper1's scoped anchor-name (got x={}, expected {})",
    overlay1_bounds.x(),
    overlay1_expected_x,
  );
  assert!(
    (overlay1_bounds.y() - overlay1_expected_y).abs() < 0.1,
    "overlay1 should resolve against wrapper1's scoped anchor-name (got y={}, expected {})",
    overlay1_bounds.y(),
    overlay1_expected_y,
  );
  assert!(
    (overlay2_bounds.x() - overlay2_expected_x).abs() < 0.1,
    "overlay2 should resolve against wrapper2's scoped anchor-name (got x={}, expected {})",
    overlay2_bounds.x(),
    overlay2_expected_x,
  );
  assert!(
    (overlay2_bounds.y() - overlay2_expected_y).abs() < 0.1,
    "overlay2 should resolve against wrapper2's scoped anchor-name (got y={}, expected {})",
    overlay2_bounds.y(),
    overlay2_expected_y,
  );
}

#[test]
fn anchor_positioning_scoped_anchors_are_not_visible_outside_the_scope_subtree() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let wrapper_id = 30usize;
  let overlay_id = 31usize;

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.width = Some(Length::px(100.0));
  wrapper_style.height = Some(Length::px(40.0));
  wrapper_style.width_keyword = None;
  wrapper_style.height_keyword = None;
  wrapper_style.anchor_names = vec!["--a".to_string()];
  wrapper_style.anchor_scope = AnchorScope::Names(vec!["--a".to_string()]);
  let mut wrapper = BoxNode::new_block(
    Arc::new(wrapper_style),
    FormattingContextType::Block,
    vec![],
  );
  wrapper.id = wrapper_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: Some(Length::px(9.0)),
  });
  overlay_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: Some(Length::px(7.0)),
  });
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![wrapper, overlay],
  );
  container.id = 110;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");
  assert!(
    (overlay_fragment.bounds.x() - 7.0).abs() < 0.1,
    "anchors scoped to a sibling subtree should not be visible (got x={})",
    overlay_fragment.bounds.x()
  );
  assert!(
    (overlay_fragment.bounds.y() - 9.0).abs() < 0.1,
    "anchors scoped to a sibling subtree should not be visible (got y={})",
    overlay_fragment.bounds.y()
  );
}

#[test]
fn anchor_positioning_position_anchor_auto_uses_implicit_anchor_for_pseudo_elements() {
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

  let parent_id = 1usize;
  let pseudo_id = 2usize;

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(50.0));
  parent_style.height = Some(Length::px(20.0));
  parent_style.width_keyword = None;
  parent_style.height_keyword = None;
  let parent_style = Arc::new(parent_style);

  let mut pseudo_style = ComputedStyle::default();
  pseudo_style.display = Display::Block;
  pseudo_style.position = Position::Absolute;
  pseudo_style.position_anchor = PositionAnchor::Auto;
  pseudo_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: None,
  });
  pseudo_style.left = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Left,
    fallback: None,
  });
  pseudo_style.width = Some(Length::px(10.0));
  pseudo_style.height = Some(Length::px(10.0));
  pseudo_style.width_keyword = None;
  pseudo_style.height_keyword = None;
  let mut pseudo = BoxNode::new_block(Arc::new(pseudo_style), FormattingContextType::Block, vec![]);
  pseudo.id = pseudo_id;
  pseudo.generated_pseudo = Some(GeneratedPseudoElement::Before);
  pseudo.implicit_anchor_box_id = Some(parent_id);

  let mut parent = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![pseudo]);
  parent.id = parent_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![parent]);
  container.id = 200;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let parent_bounds = find_abs_bounds_by_box_id(&fragment, parent_id).expect("parent bounds");
  let pseudo_bounds = find_abs_bounds_by_box_id(&fragment, pseudo_id).expect("pseudo bounds");

  assert!(
    (pseudo_bounds.x() - parent_bounds.x()).abs() < 0.1,
    "pseudo left should resolve against the originating element's left edge"
  );
  assert!(
    (pseudo_bounds.y() - parent_bounds.max_y()).abs() < 0.1,
    "pseudo top should resolve against the originating element's bottom edge"
  );
}

#[test]
fn anchor_positioning_position_anchor_auto_uses_fallback_when_no_implicit_anchor_exists() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  let container_style = Arc::new(container_style);

  let overlay_id = 1usize;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Auto;
  overlay_style.top = InsetValue::Anchor(AnchorFunction {
    name: None,
    side: AnchorSide::Bottom,
    fallback: Some(Length::px(123.0)),
  });
  overlay_style.left = InsetValue::Length(Length::px(0.0));
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(
    Arc::new(overlay_style),
    FormattingContextType::Block,
    vec![],
  );
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![overlay]);
  container.id = 201;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let overlay_bounds = find_abs_bounds_by_box_id(&fragment, overlay_id).expect("overlay bounds");
  assert!(
    (overlay_bounds.y() - 123.0).abs() < 0.1,
    "non-pseudo elements with position-anchor:auto should fall back when no implicit anchor exists (got y={})",
    overlay_bounds.y()
  );
}
