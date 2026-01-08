use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::geometry::Size;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::{
  AlignContent, AlignItems, BorderStyle, BoxSizing, Direction, FlexDirection, FlexWrap, JustifyContent,
  WritingMode,
};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn abs_child_position(fragment: &FragmentNode) -> (f32, f32) {
  let abs_fragment = fragment.children.iter().find(|child| {
    matches!(
      child.style.as_ref().map(|s| s.position),
      Some(Position::Absolute)
    )
  });
  let Some(abs_fragment) = abs_fragment else {
    let debug_children: Vec<_> = fragment
      .children
      .iter()
      .map(|child| {
        (
          child.style.as_ref().map(|s| s.position),
          child.bounds.x(),
          child.bounds.y(),
          child.bounds.width(),
          child.bounds.height(),
        )
      })
      .collect();
    panic!("absolute fragment present; children={debug_children:?}");
  };
  (abs_fragment.bounds.x(), abs_fragment.bounds.y())
}

fn layout_abspos_child_in_size(
  container_style: ComputedStyle,
  child_style: ComputedStyle,
  width: f32,
  height: f32,
) -> (f32, f32) {
  let abs_child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![abs_child],
  );
  assert!(
    matches!(container.children[0].style.position, Position::Absolute),
    "test setup requires a single absolute-positioned child"
  );
  let constraints = LayoutConstraints::definite(width, height);
  let fc = FlexFormattingContext::new();

  let first = fc.layout(&container, &constraints).expect("flex layout");
  let first_pos = abs_child_position(&first);

  // Run layout again to guard against cache-order / template reuse changing the static position.
  let second = fc.layout(&container, &constraints).expect("flex layout");
  let second_pos = abs_child_position(&second);
  assert!(
    (first_pos.0 - second_pos.0).abs() < 1e-3 && (first_pos.1 - second_pos.1).abs() < 1e-3,
    "abspos static position should be stable across layout calls (first={first_pos:?}, second={second_pos:?})"
  );

  first_pos
}

fn layout_abspos_child(container_style: ComputedStyle, child_style: ComputedStyle) -> (f32, f32) {
  layout_abspos_child_in_size(container_style, child_style, 100.0, 100.0)
}

#[test]
fn abspos_static_position_respects_center_alignment() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::Center;
  container_style.align_items = AlignItems::Center;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(20.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 45.0).abs() < 0.1, "expected x≈45, got {}", x);
  assert!((y - 40.0).abs() < 0.1, "expected y≈40, got {}", y);
}

#[test]
fn abspos_static_position_respects_flex_end_alignment() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_space_between_fallback_for_single_item() {
  // Flexbox: `justify-content: space-between` falls back to `flex-start` when there is a single
  // flex item. The abspos static position is computed as if the child were the sole flex item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_respects_space_between_fallback_in_row_reverse() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_row_reverse_main_start() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_justify_content_start_in_row_reverse() {
  // `justify-content: start` resolves against the container's inline-start edge and should not be
  // affected by `flex-direction: row-reverse` (unlike `flex-start`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::Start;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_respects_column_reverse_main_start() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_justify_content_start_in_column_reverse() {
  // `justify-content: start` resolves against the block-start edge and is not affected by
  // `flex-direction: column-reverse`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::Start;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_rtl_row_reverse_flex_start() {
  // In RTL, `flex-direction: row-reverse` makes the main axis physical direction left-to-right.
  // `justify-content: flex-start` aligns to the main-start edge (left).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_respects_rtl_row_reverse_start_keyword() {
  // `justify-content: start` always resolves against the inline-start edge (right in RTL),
  // regardless of `row-reverse`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::Start;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_rtl_main_start() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.direction = Direction::Rtl;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_vertical_writing_mode_axes() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::Start;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_wrap_in_negative_cross_axis_writing_mode() {
  // Our flex adapter emulates negative-physical cross axes for wrapping containers (including
  // vertical writing modes) by mirroring after Taffy layout. Abspos static-position probing must
  // apply the same mirroring.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_wrap_reverse_cross_axis_direction() {
  // Flexbox §flex-wrap: `wrap-reverse` swaps cross-start/cross-end, which affects `align-items` and
  // therefore the cross-axis static position for abspos flex children (Flexbox §abspos-items).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_ignores_wrap_reverse_for_start_keyword() {
  // `wrap-reverse` flips the flex cross-start/cross-end edges, but the `start` keyword resolves
  // against the container's physical writing-mode axis and must not mirror with flex lines.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::Start;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_wrap_reverse_with_horizontal_cross_axis() {
  // Same as above, but with a horizontal cross axis (`flex-direction: column`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_ignores_wrap_reverse_for_start_keyword_with_horizontal_cross_axis() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::Start;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_ignores_align_content() {
  // Flexbox §abspos-items: the cross-axis edges of the static-position rectangle are the flex
  // container's content edges (and thus ignore `align-content`).
  //
  // If `align-content` were applied to the sole flex line, `align-content:center` in a wrapping
  // container would center the line (and therefore the abspos child) at y≈45. The spec says the
  // abspos child should instead align against the content box edges (y≈0 for `align-items:flex-start`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = AlignContent::Center;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_accounts_for_cross_axis_margins() {
  // The abspos static-position rectangle aligns the child's *margin box* on the main axis and uses
  // `align-items`/`align-self` to align on the cross axis. This means cross-axis margins affect the
  // border box position.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::Center;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.margin_top = Some(Length::px(20.0));
  child_style.margin_bottom = Some(Length::px(0.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  // Margin box height = 20(top) + 10 + 0(bottom) = 30; centered in 100 → margin edge at 35.
  // Border box y = margin edge + margin-top = 35 + 20 = 55.
  assert!((y - 55.0).abs() < 0.1, "expected y≈55, got {}", y);
}

#[test]
fn abspos_static_position_accounts_for_main_axis_margins_in_column_flex() {
  // Same as above, but with a vertical main axis (`flex-direction: column`). The main-axis
  // static-position rectangle edges are defined using the child's margin edges.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.margin_top = Some(Length::px(20.0));
  child_style.margin_bottom = Some(Length::px(0.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 20.0).abs() < 0.1, "expected y≈20, got {}", y);
}

#[test]
fn abspos_static_position_treats_cross_axis_auto_margins_as_zero() {
  // Flexbox §abspos-items: for determining the static-position rectangle, auto margins are treated
  // as zero. In particular, cross-axis auto margins must not center the item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.margin_top = None; // auto
  child_style.margin_bottom = None; // auto

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_resolves_block_axis_percentage_margins_against_container_width() {
  // CSS 2.1 §8.3: percentage margins resolve against the containing block width on both axes.
  //
  // Use asymmetric container sizes to catch implementations that incorrectly resolve vertical
  // margins against the block size.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::Center;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.margin_top = Some(Length::percent(50.0)); // 50% of CB width = 100px

  let (_, y) = layout_abspos_child_in_size(container_style, child_style, 200.0, 100.0);
  // Margin box height is 100 + 10 + 0 = 110, so centering in 100 yields a margin-edge y of -5.
  // Absolute positioning then adds margin-top (100), producing a border box y of 95.
  assert!((y - 95.0).abs() < 0.1, "expected y≈95, got {}", y);
}

#[test]
fn abspos_static_position_uses_used_size_for_auto_main_size() {
  // Flexbox §abspos-items defines the main-axis edges of the static-position rectangle using the
  // abspos child's *used size*. For `width:auto` abspos boxes this is shrink-to-fit, which can
  // differ from the hypothetical in-flow size used to measure intrinsic sizes.
  //
  // Regression: if the static-position probe uses the child's in-flow border box size (100px),
  // `justify-content:center` produces x=0, and the abspos child ends up left-aligned even though
  // its used size is smaller.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::Center;
  container_style.align_items = AlignItems::FlexStart;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.height = Some(Length::px(10.0));

  let replaced_style = ComputedStyle::default();
  let replaced_child = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(20.0, 10.0)),
    None,
  );
  let abs_child = BoxNode::new_block(
    Arc::new(abs_style),
    FormattingContextType::Block,
    vec![replaced_child],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![abs_child],
  );
  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = FlexFormattingContext::new();

  let fragment = fc.layout(&container, &constraints).expect("flex layout");
  let (x, _) = abs_child_position(&fragment);
  assert!((x - 40.0).abs() < 0.1, "expected x≈40, got {}", x);
}

#[test]
fn abspos_static_position_respects_align_self_on_cross_axis() {
  // Flexbox § abspos-items: the cross-axis edges of the static-position rectangle are the flex
  // container's content edges, and `align-self` is used to align within that axis.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.align_self = Some(AlignItems::FlexEnd);

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_align_self_self_start_with_different_direction() {
  // `self-start` resolves against the item's own writing-mode/direction rather than the flex
  // container's. Use a column flex container (horizontal cross axis) and give the abspos child an
  // RTL direction, making `self-start` the right edge.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.direction = Direction::Rtl;
  child_style.align_self = Some(AlignItems::SelfStart);

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_ignores_justify_self_on_main_axis() {
  // Flexbox § abspos-items defines the main-axis edges of the static-position rectangle as where
  // the child's margin edges would be if it were the sole flex item. That makes `justify-self`
  // irrelevant for the main-axis static position (WPT: flex-abspos-staticpos-justify-self-001).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::Center;
  container_style.align_items = AlignItems::Center;
  container_style.justify_items = AlignItems::End;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.justify_self = Some(AlignItems::FlexEnd);

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 45.0).abs() < 0.1, "expected x≈45, got {}", x);
  assert!((y - 45.0).abs() < 0.1, "expected y≈45, got {}", y);
}

#[test]
fn abspos_static_position_is_relative_to_flex_content_box() {
  // The "static position rectangle" for abspos flex children is the flex container's content box
  // (i.e. inside padding/border). Ensure we measure alignment from there rather than from the
  // border box.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::Center;
  container_style.align_items = AlignItems::Center;

  container_style.border_left_style = BorderStyle::Solid;
  container_style.border_right_style = BorderStyle::Solid;
  container_style.border_top_style = BorderStyle::Solid;
  container_style.border_bottom_style = BorderStyle::Solid;
  container_style.border_left_width = Length::px(2.0);
  container_style.border_right_width = Length::px(8.0);
  container_style.border_top_width = Length::px(4.0);
  container_style.border_bottom_width = Length::px(6.0);

  container_style.padding_left = Length::px(5.0);
  container_style.padding_right = Length::px(15.0);
  container_style.padding_top = Length::px(7.0);
  container_style.padding_bottom = Length::px(9.0);

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(20.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  // Content box is 100 - (2+8) - (5+15) = 70 wide, 100 - (4+6) - (7+9) = 74 tall.
  // Center alignment therefore places a 10×20 child at:
  // x = border_left + padding_left + (70 - 10)/2 = 2 + 5 + 30 = 37
  // y = border_top + padding_top + (74 - 20)/2 = 4 + 7 + 27 = 38
  assert!((x - 37.0).abs() < 0.1, "expected x≈37, got {}", x);
  assert!((y - 38.0).abs() < 0.1, "expected y≈38, got {}", y);
}
