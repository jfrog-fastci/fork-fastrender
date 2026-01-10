use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, FlexWrap};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::tree::box_tree::BoxNode;
use fastrender::{ComputedStyle, FormattingContext};

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  let matches_id = matches!(
    fragment.content,
    FragmentContent::Block { box_id: Some(box_id) }
      | FragmentContent::Inline { box_id: Some(box_id), .. }
      | FragmentContent::Text { box_id: Some(box_id), .. }
      | FragmentContent::Replaced { box_id: Some(box_id), .. }
      if box_id == id
  );
  if matches_id {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_by_box_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn fixed_block(width: f32, id: usize) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  style.width_keyword = None;
  style.height_keyword = None;
  // Avoid flexing so widths flow through as intrinsic sizes.
  style.flex_shrink = 0.0;
  let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  node.id = id;
  node
}

fn intrinsic_item(width: f32, item_id: usize, child_id: usize) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.flex_shrink = 0.0;
  style.width_keyword = None;
  style.height_keyword = None;
  let child = fixed_block(width, child_id);
  let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![child]);
  node.id = item_id;
  node
}

#[test]
fn flex_intrinsic_inline_size_accounts_for_flex_item_snapping() {
  // Regression: when a flex container is measured as a flex item, Taffy asks the measure callback
  // for its max-content contribution. FastRender answers that probe using the flex formatting
  // context's intrinsic sizing API and then snaps the result to whole pixels.
  //
  // Taffy later performs line-breaking inside the nested flex container using hypothetical sizes of
  // its own flex items; those sizes account for per-item probe snapping, and can therefore be
  // slightly larger than a raw sum of max-content widths. If the nested container's max-content
  // probe underestimates, the nested flex container can wrap even though its items would fit once
  // snapping is applied.
  let fc = FlexFormattingContext::new();

  let item_width = 10.6;
  let item1 = intrinsic_item(item_width, 2, 3);
  let item2 = intrinsic_item(item_width, 4, 5);

  let mut nav_style = ComputedStyle::default();
  nav_style.display = Display::Flex;
  nav_style.flex_direction = FlexDirection::Row;
  nav_style.flex_wrap = FlexWrap::Wrap;
  // Prevent the outer container from shrinking the nested flex container; we want the nested
  // container's own max-content probe to drive its used width.
  nav_style.flex_shrink = 0.0;

  let mut nav = BoxNode::new_block(
    Arc::new(nav_style),
    FormattingContextType::Flex,
    vec![item1, item2],
  );
  nav.id = 1;

  let raw_item = fc
    .compute_intrinsic_inline_size(&nav.children[0], IntrinsicSizingMode::MaxContent)
    .expect("item intrinsic inline size");
  let raw_nav = fc
    .compute_intrinsic_inline_size(&nav, IntrinsicSizingMode::MaxContent)
    .expect("nav intrinsic inline size");
  let snapped_items_sum = raw_item.round() * 2.0;
  assert!(
    snapped_items_sum > raw_nav.round() + 0.1,
    "expected nested container raw max-content width ({raw_nav:.2}) to round below the sum of per-item snapped widths ({snapped_items_sum:.2}); item={raw_item:.2}"
  );

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Row;
  outer_style.width = Some(Length::px(200.0));
  outer_style.height = Some(Length::px(50.0));
  outer_style.width_keyword = None;
  outer_style.height_keyword = None;

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![nav],
  );

  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");

  let nav_fragment =
    find_fragment_by_box_id(&fragment, 1).unwrap_or_else(|| panic!("missing nav fragment: {fragment:#?}"));
  let item2_fragment = find_fragment_by_box_id(&fragment, 4)
    .unwrap_or_else(|| panic!("missing second nav item fragment: {fragment:#?}"));

  let y_offset = item2_fragment.bounds.y() - nav_fragment.bounds.y();
  assert!(
    y_offset.abs() <= 0.5,
    "expected nested flex container not to wrap; second item y offset={y_offset:.2} (nav_width={:.2}, raw_nav={raw_nav:.2}, raw_item={raw_item:.2})",
    nav_fragment.bounds.width(),
  );
}
