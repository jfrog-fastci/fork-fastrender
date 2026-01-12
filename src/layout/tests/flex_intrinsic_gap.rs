use std::sync::Arc;

use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, FlexWrap};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::{BoxNode, ReplacedType};
use fastrender::{ComputedStyle, FormattingContext, FragmentNode};

fn fixed_block(width: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn flex_intrinsic_inline_size_includes_column_gap_between_items() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.grid_column_gap = Length::px(8.0);
  container_style.grid_column_gap_is_normal = false;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed_block(10.0), fixed_block(20.0), fixed_block(30.0)],
  );

  let fc = FlexFormattingContext::new();
  let width = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  // Flexbox intrinsic sizing considers the sum of item contributions plus the column gaps between
  // them (2 gaps at 8px each).
  assert!(
    (width - 76.0).abs() < 0.01,
    "expected width≈76, got {width}"
  );
}

fn assert_approx(actual: f32, expected: f32, epsilon: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= epsilon,
    "{msg}: expected {expected}, got {actual}"
  );
}

fn find_block_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| {
      panic!(
        "missing fragment for box_id={box_id}; got children ids={:?}",
        fragment
          .children
          .iter()
          .map(|c| c.box_id())
          .collect::<Vec<_>>()
      )
    })
}

fn build_icon(id: usize) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.flex_shrink = 0.0;
  let mut node = BoxNode::new_replaced(
    Arc::new(style),
    ReplacedType::Canvas,
    Some(Size::new(36.0, 20.0)),
    None,
  );
  node.id = id;
  node
}

fn build_engine(id: usize, icon_count: usize) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.flex_wrap = FlexWrap::Wrap;
  // Mirrors the MDN baseline indicator pills:
  // padding: .5rem .625rem; gap: .5rem;
  style.padding_top = Length::px(8.0);
  style.padding_bottom = Length::px(8.0);
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  style.grid_row_gap = Length::px(8.0);
  style.grid_column_gap = Length::px(8.0);
  style.grid_row_gap_is_normal = false;
  style.grid_column_gap_is_normal = false;

  let children = (0..icon_count).map(|i| build_icon(id * 10 + i)).collect();
  let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, children);
  node.id = id;
  node
}

#[test]
fn flex_intrinsic_inline_size_accounts_for_gap_between_items() {
  // Regression test for flex intrinsic sizing: when a flex container is itself a flex item,
  // its max-content inline size must include `column-gap` between in-flow children. If the gap is
  // ignored, the flex item is measured too small and may wrap unexpectedly.
  //
  // This is exercised by the MDN `text-combine-upright` page's baseline indicator pills, where the
  // Blink engine pill contains two browser icons separated by a gap.
  let fc = FlexFormattingContext::new();

  let mut browsers_style = ComputedStyle::default();
  browsers_style.display = Display::Flex;
  browsers_style.flex_wrap = FlexWrap::Wrap;
  browsers_style.width = Some(Length::px(204.0));
  browsers_style.width_keyword = None;
  browsers_style.grid_row_gap = Length::px(8.0);
  browsers_style.grid_column_gap = Length::px(8.0);
  browsers_style.grid_row_gap_is_normal = false;
  browsers_style.grid_column_gap_is_normal = false;

  let engine_blink = build_engine(1, 2);
  let engine_gecko = build_engine(2, 1);
  let engine_webkit = build_engine(3, 1);

  let browsers = BoxNode::new_block(
    Arc::new(browsers_style),
    FormattingContextType::Flex,
    vec![engine_blink, engine_gecko, engine_webkit],
  );

  let fragment = fc
    .layout(&browsers, &LayoutConstraints::definite_width(204.0))
    .expect("layout succeeds");

  let blink = find_block_child(&fragment, 1);
  // Two 36px icons + 8px gap + 20px horizontal padding.
  assert_approx(blink.bounds.width(), 100.0, 0.5, "blink pill width");
  // 20px icon height + 16px vertical padding.
  assert_approx(blink.bounds.height(), 36.0, 0.5, "blink pill height");

  let first_icon = find_block_child(blink, 10);
  let second_icon = find_block_child(blink, 11);
  // Both icons should be on the same row (no unexpected wrapping).
  assert_approx(first_icon.bounds.y(), 8.0, 0.5, "first icon y");
  assert_approx(second_icon.bounds.y(), 8.0, 0.5, "second icon y");
  assert_approx(second_icon.bounds.x(), 54.0, 0.5, "second icon x");
}
