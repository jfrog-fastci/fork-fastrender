use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{AnchorFunction, AnchorSide, InsetValue, PositionAnchor};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
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
