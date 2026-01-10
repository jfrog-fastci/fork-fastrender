use std::sync::Arc;

use fastrender::css::types::{Declaration, PropertyName, PropertyValue};
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::position_try::PositionTryRegistry;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{PositionAnchor, PositionArea, WritingMode};
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
fn position_area_block_start_places_above_anchor_and_centers_inline() {
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
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.margin_top = Some(Length::px(100.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.position_area = PositionArea::parse("block-start").expect("parse position-area");
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![anchor, overlay]);
  container.id = 200;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.max_y() - anchor_fragment.bounds.y()).abs() < 0.1,
    "block-start should place overlay above anchor (overlay max_y={}, anchor y={})",
    overlay_fragment.bounds.max_y(),
    anchor_fragment.bounds.y()
  );

  let anchor_center_x = anchor_fragment.bounds.x() + anchor_fragment.bounds.width() / 2.0;
  let overlay_center_x = overlay_fragment.bounds.x() + overlay_fragment.bounds.width() / 2.0;
  assert!(
    (overlay_center_x - anchor_center_x).abs() < 0.1,
    "span-all should anchor-center on the inline axis (overlay cx={}, anchor cx={})",
    overlay_center_x,
    anchor_center_x
  );
}

#[test]
fn position_area_flip_block_try_fallback_switches_to_block_end() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(80.0));
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
  anchor_style.margin_left = Some(Length::px(40.0));
  anchor_style.margin_top = Some(Length::px(5.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.position_area = PositionArea::parse("block-start").expect("parse position-area");
  overlay_style.position_try_fallbacks = vec!["flip-block".to_string()];
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(20.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![anchor, overlay]);
  container.id = 201;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 80.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "flip-block should move overlay below the anchor when block-start overflows (overlay y={}, anchor max_y={})",
    overlay_fragment.bounds.y(),
    anchor_fragment.bounds.max_y()
  );
  assert!(
    overlay_fragment.bounds.y() >= -0.1,
    "overlay should not overflow the container after flipping (overlay y={})",
    overlay_fragment.bounds.y()
  );
}

#[test]
fn position_area_position_try_rule_changes_area_across_candidates() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(80.0));
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
  anchor_style.margin_left = Some(Length::px(40.0));
  anchor_style.margin_top = Some(Length::px(5.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut registry = PositionTryRegistry::default();
  registry.register(
    "--below".to_string(),
    vec![Declaration {
      property: PropertyName::from("position-area"),
      value: PropertyValue::Keyword("block-end".to_string()),
      raw_value: String::new(),
      important: false,
      contains_var: false,
    }],
  );

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.position_area = PositionArea::parse("block-start").expect("parse position-area");
  overlay_style.position_try_fallbacks = vec!["--below".to_string()];
  overlay_style.position_try_registry = Arc::new(registry);
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(20.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![anchor, overlay]);
  container.id = 203;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 80.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.y() - anchor_fragment.bounds.max_y()).abs() < 0.1,
    "@position-try should be able to override position-area and select the first candidate that fits (overlay y={}, anchor max_y={})",
    overlay_fragment.bounds.y(),
    anchor_fragment.bounds.max_y()
  );
  assert!(
    overlay_fragment.bounds.y() >= -0.1,
    "overlay should not overflow the container after applying the try rule (overlay y={})",
    overlay_fragment.bounds.y()
  );
}

#[test]
fn position_area_vertical_rl_block_start_places_on_block_start_side() {
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
  anchor_style.writing_mode = WritingMode::VerticalRl;
  anchor_style.width = Some(Length::px(20.0));
  anchor_style.height = Some(Length::px(40.0));
  anchor_style.width_keyword = None;
  anchor_style.height_keyword = None;
  anchor_style.margin_left = Some(Length::px(80.0));
  anchor_style.margin_top = Some(Length::px(60.0));
  anchor_style.anchor_names = vec!["--a".to_string()];
  let mut anchor = BoxNode::new_block(Arc::new(anchor_style), FormattingContextType::Block, vec![]);
  anchor.id = anchor_id;

  let mut overlay_style = ComputedStyle::default();
  overlay_style.display = Display::Block;
  overlay_style.position = Position::Absolute;
  overlay_style.writing_mode = WritingMode::VerticalRl;
  overlay_style.position_anchor = PositionAnchor::Name("--a".to_string());
  overlay_style.position_area = PositionArea::parse("block-start").expect("parse position-area");
  overlay_style.width = Some(Length::px(10.0));
  overlay_style.height = Some(Length::px(10.0));
  overlay_style.width_keyword = None;
  overlay_style.height_keyword = None;
  let mut overlay = BoxNode::new_block(Arc::new(overlay_style), FormattingContextType::Block, vec![]);
  overlay.id = overlay_id;

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![anchor, overlay]);
  container.id = 202;

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let anchor_fragment = find_fragment_by_box_id(&fragment, anchor_id).expect("anchor fragment");
  let overlay_fragment = find_fragment_by_box_id(&fragment, overlay_id).expect("overlay fragment");

  assert!(
    (overlay_fragment.bounds.x() - anchor_fragment.bounds.max_x()).abs() < 0.1,
    "vertical-rl block-start should place overlay on the physical right side (overlay x={}, anchor max_x={})",
    overlay_fragment.bounds.x(),
    anchor_fragment.bounds.max_x()
  );

  let anchor_center_y = anchor_fragment.bounds.y() + anchor_fragment.bounds.height() / 2.0;
  let overlay_center_y = overlay_fragment.bounds.y() + overlay_fragment.bounds.height() / 2.0;
  assert!(
    (overlay_center_y - anchor_center_y).abs() < 0.1,
    "span-all should anchor-center on the (vertical) inline axis (overlay cy={}, anchor cy={})",
    overlay_center_y,
    anchor_center_y
  );
}
