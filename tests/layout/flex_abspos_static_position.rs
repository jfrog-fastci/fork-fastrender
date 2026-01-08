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
fn abspos_static_position_allows_negative_main_axis_offset_when_item_overflows() {
  // With negative free space, `justify-content:center` should still center the item, producing a
  // negative start offset when the item is wider than the container.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::Center;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - (-10.0)).abs() < 0.1, "expected x≈-10, got {}", x);
}

#[test]
fn abspos_static_position_allows_negative_cross_axis_offset_when_item_overflows() {
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
  child_style.height = Some(Length::px(120.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - (-10.0)).abs() < 0.1, "expected y≈-10, got {}", y);
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
fn abspos_static_position_respects_space_around_fallback_for_single_item() {
  // Flexbox: `justify-content: space-around` falls back to `safe center` with a single item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::SpaceAround;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 45.0).abs() < 0.1, "expected x≈45, got {}", x);
}

#[test]
fn abspos_static_position_respects_space_evenly_fallback_for_single_item() {
  // Box Alignment: `justify-content: space-evenly` also centers a single item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::SpaceEvenly;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 45.0).abs() < 0.1, "expected x≈45, got {}", x);
}

#[test]
fn abspos_static_position_respects_space_between_fallback_for_single_item_in_column() {
  // `space-between` falls back to `flex-start` in the main axis. Cover a vertical main axis.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_space_between_fallback_in_column_reverse() {
  // Same as above, but with a reversed main axis.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_space_around_fallback_for_single_item_in_column() {
  // `space-around` falls back to `safe center` with a single item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::SpaceAround;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 45.0).abs() < 0.1, "expected y≈45, got {}", y);
}

#[test]
fn abspos_static_position_respects_space_evenly_fallback_for_single_item_in_column() {
  // `space-evenly` centers a single item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::SpaceEvenly;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 45.0).abs() < 0.1, "expected y≈45, got {}", y);
}

#[test]
fn abspos_static_position_space_between_negative_free_space_falls_back_to_safe_start() {
  // Flexbox §justify-content: with negative free space, `space-between` falls back to `safe flex-start`.
  // Safe overflow alignment causes the item to start-align to the physical start edge (not main-start).
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
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_space_evenly_negative_free_space_falls_back_to_safe_start() {
  // Box Alignment: `justify-content: space-evenly` falls back to `safe center` with negative free space.
  // (Safe overflow alignment -> physical start.)
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::SpaceEvenly;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_space_evenly_negative_free_space_falls_back_to_safe_start_in_row_reverse() {
  // Like the test above, but with a reversed main axis. The fallback is `safe center`, which under
  // safe overflow alignment becomes physical start (not main-start), so the item should start-align
  // to x≈0 even though `row-reverse` main-start is on the right.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::SpaceEvenly;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_space_around_negative_free_space_falls_back_to_safe_start() {
  // Flexbox §justify-content: with negative free space, `space-around` falls back to `safe center`,
  // which becomes start-aligned under safe overflow alignment.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.justify_content = JustifyContent::SpaceAround;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_space_around_negative_free_space_falls_back_to_safe_start_in_row_reverse() {
  // With negative free space, `space-around` falls back to `safe center`, which becomes physical
  // start under safe overflow alignment. Ensure this uses physical start even when the main axis is
  // reversed.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::SpaceAround;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(120.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_space_between_negative_free_space_falls_back_to_safe_start_in_column_reverse() {
  // Same as `abspos_static_position_space_between_negative_free_space_falls_back_to_safe_start`, but
  // with a vertical (reversed) main axis.
  //
  // With `flex-direction: column-reverse` the main-start edge is on the bottom, but the safe
  // fallback must still align to the physical start edge (top) when the item overflows.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(120.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_space_evenly_negative_free_space_falls_back_to_safe_start_in_column_reverse() {
  // Like the space-between test above, but for the Box Alignment `space-evenly` keyword.
  // `space-evenly` falls back to `safe center` under negative free space, which becomes physical
  // start alignment.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::SpaceEvenly;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(120.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_space_around_negative_free_space_falls_back_to_safe_start_in_column_reverse() {
  // `space-around` falls back to `safe center` under negative free space, which becomes physical
  // start alignment. Cover a reversed vertical main axis.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::SpaceAround;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(120.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
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
fn abspos_static_position_respects_row_reverse_main_end() {
  // `justify-content:flex-end` aligns to the main-end edge (left) in `row-reverse`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
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
fn abspos_static_position_respects_justify_content_end_in_row_reverse() {
  // `justify-content: end` resolves against the container's inline-end edge and should not be
  // affected by `flex-direction: row-reverse` (unlike `flex-end`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::End;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
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
fn abspos_static_position_respects_column_reverse_main_end() {
  // `justify-content:flex-end` aligns to the main-end edge (top) in `column-reverse`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
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
fn abspos_static_position_respects_justify_content_end_in_column_reverse() {
  // `justify-content: end` resolves against the block-end edge and is not affected by
  // `flex-direction: column-reverse`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::End;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
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
fn abspos_static_position_respects_rtl_row_reverse_flex_end() {
  // In RTL + `row-reverse`, the main-end edge is on the right.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
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
fn abspos_static_position_respects_rtl_justify_content_end() {
  // In RTL, inline-end is physical left, so `justify-content: end` should align to x≈0.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.direction = Direction::Rtl;
  container_style.justify_content = JustifyContent::End;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
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
fn abspos_static_position_respects_vertical_lr_writing_mode_axes() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::Start;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_respects_rtl_direction_in_vertical_writing_mode_for_flex_end() {
  // In vertical writing modes, `direction` flips the inline-start/inline-end edges even though the
  // inline axis is vertical.
  //
  // With `writing-mode: vertical-rl` + `direction: rtl`, the inline-end edge is physical top. A row
  // flex container's main axis is the inline axis, so `justify-content:flex-end` should align to
  // y≈0 (top), not to the bottom edge.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
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
fn abspos_static_position_respects_rtl_direction_in_vertical_lr_writing_mode_for_flex_end() {
  // Same as `abspos_static_position_respects_rtl_direction_in_vertical_writing_mode_for_flex_end`,
  // but with `writing-mode: vertical-lr` (so the block axis is physical left-to-right).
  //
  // `direction: rtl` still flips the inline-start/inline-end edges even though the inline axis is
  // vertical, so `justify-content:flex-end` should align to the physical top edge (y≈0).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_start_keyword_in_vertical_writing_mode_row_reverse() {
  // In vertical writing mode, `flex-direction: row-reverse` reverses the main axis (inline axis),
  // but `justify-content: start` should still align to the inline-start edge (top).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::Start;
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
fn abspos_static_position_respects_wrap_in_vertical_lr_writing_mode() {
  // `writing-mode: vertical-lr` has a positive-physical block axis. Ensure the wrapping flex adapter
  // does not incorrectly apply cross-axis mirroring in this mode (unlike vertical-rl).
  for (align_items, expected_x) in [(AlignItems::FlexStart, 0.0), (AlignItems::FlexEnd, 90.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalLr;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {y}");
  }
}

#[test]
fn abspos_static_position_respects_wrap_reverse_in_vertical_lr_writing_mode() {
  // In vertical-lr, the block-start edge is physical left and `wrap-reverse` swaps cross-start to
  // the physical right edge. Cover the cross-axis static position for abspos children.
  for (align_items, expected_x) in [(AlignItems::FlexStart, 90.0), (AlignItems::FlexEnd, 0.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalLr;
    container_style.flex_wrap = FlexWrap::WrapReverse;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {y}");
  }
}

#[test]
fn abspos_static_position_ignores_wrap_reverse_for_start_end_keywords_in_vertical_lr_writing_mode() {
  // `start`/`end` align to the writing-mode block-start/block-end edges and must not mirror with
  // `wrap-reverse` (unlike `flex-start`/`flex-end`).
  for (align_items, expected_x) in [(AlignItems::Start, 0.0), (AlignItems::End, 90.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalLr;
    container_style.flex_wrap = FlexWrap::WrapReverse;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {y}");
  }
}

#[test]
fn abspos_static_position_respects_start_end_keywords_in_vertical_writing_mode_rtl_row_reverse() {
  // Same as the previous test, but with `direction: rtl` which flips the inline-start edge to the
  // bottom in vertical writing modes.
  for (justify, expected_y) in [(JustifyContent::Start, 90.0), (JustifyContent::End, 0.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.direction = Direction::Rtl;
    container_style.flex_direction = FlexDirection::RowReverse;
    container_style.justify_content = justify;
    container_style.align_items = AlignItems::FlexStart;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (_, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (y - expected_y).abs() < 0.1,
      "expected y≈{expected_y} for justify-content={justify:?}, got {y}"
    );
  }
}

#[test]
fn abspos_static_position_respects_start_end_keywords_in_vertical_writing_mode_column_reverse() {
  // In vertical writing mode, `flex-direction: column-reverse` reverses the main axis (block axis),
  // but `justify-content: start/end` should still align to the block-start/block-end edges.
  for (justify, expected_x) in [(JustifyContent::Start, 90.0), (JustifyContent::End, 0.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.flex_direction = FlexDirection::ColumnReverse;
    container_style.justify_content = justify;
    container_style.align_items = AlignItems::FlexStart;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for justify-content={justify:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
  }
}

#[test]
fn abspos_static_position_ignores_wrap_mirroring_for_start_end_keywords_in_vertical_writing_mode() {
  // Wrapping containers in vertical-rl have a negative physical cross axis, so the flex adapter
  // mirrors after Taffy layout. `start`/`end` are physical keywords and must *not* mirror with that
  // post-pass.
  for (align, expected_x) in [(AlignItems::Start, 90.0), (AlignItems::End, 0.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, _) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align={align:?}, got {x}"
    );
  }
}

#[test]
fn abspos_static_position_ignores_wrap_mirroring_for_start_end_keywords_in_vertical_writing_mode_on_align_self() {
  // Same as above, but with `align-self` overriding the container's `align-items`.
  for (align_self, expected_x) in [(AlignItems::Start, 90.0), (AlignItems::End, 0.0)] {
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
    child_style.align_self = Some(align_self);

    let (x, _) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-self={align_self:?}, got {x}"
    );
  }
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
fn abspos_static_position_respects_wrap_in_negative_cross_axis_writing_mode_for_flex_end() {
  // Same as `abspos_static_position_respects_wrap_in_negative_cross_axis_writing_mode`, but with
  // `align-items:flex-end` which aligns to the cross-end edge (physical left) after the mirror.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_wrap_reverse_in_negative_cross_axis_writing_mode() {
  // Same as above, but with `flex-wrap: wrap-reverse` which swaps cross-start/cross-end. In
  // vertical-rl the physical cross axis is negative by default (block-start is right), but
  // wrap-reverse swaps it back to a positive direction, so no mirroring should occur.
  for (align_items, expected_x) in [(AlignItems::FlexStart, 0.0), (AlignItems::FlexEnd, 90.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.flex_wrap = FlexWrap::WrapReverse;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {y}");
  }
}

#[test]
fn abspos_static_position_respects_wrap_reverse_in_negative_cross_axis_writing_mode_on_align_self() {
  // Same as above, but with `align-self` overriding the container's `align-items`.
  for (align_self, expected_x) in [(AlignItems::FlexStart, 0.0), (AlignItems::FlexEnd, 90.0)] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.flex_wrap = FlexWrap::WrapReverse;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = AlignItems::Center;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));
    child_style.align_self = Some(align_self);

    let (x, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-self={align_self:?}, got {x}"
    );
    assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {y}");
  }
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
fn abspos_static_position_respects_wrap_reverse_cross_axis_direction_inside_content_box() {
  // Like `abspos_static_position_respects_wrap_reverse_cross_axis_direction`, but ensure the
  // wrap-reverse cross axis flip is computed relative to the flex container's content box (inside
  // border + padding).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

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
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  // Content box is 70×74. Under wrap-reverse, cross-start is content bottom: y = 4+7+(74-10)=75.
  assert!((x - 7.0).abs() < 0.1, "expected x≈7, got {}", x);
  assert!((y - 75.0).abs() < 0.1, "expected y≈75, got {}", y);
}

#[test]
fn abspos_static_position_respects_align_items_flex_end_under_wrap_reverse() {
  // Under `wrap-reverse` in a horizontal writing mode, the flex cross-end edge is the physical top
  // edge, so `align-items:flex-end` should place the child at y≈0.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_align_self_flex_start_under_wrap_reverse() {
  // `align-self` should override `align-items` and still respect the wrap-reverse cross-start edge
  // for the `flex-start` keyword.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.align_self = Some(AlignItems::FlexStart);

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
fn abspos_static_position_ignores_wrap_reverse_for_end_keyword() {
  // `end` is physical and must not mirror with wrap-reverse (unlike `flex-end`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::End;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 90.0).abs() < 0.1, "expected y≈90, got {}", y);
}

#[test]
fn abspos_static_position_ignores_wrap_reverse_for_start_end_keywords_on_align_self() {
  // Like the two tests above, but with `align-self` overriding `align-items`.
  for (align_self, expected_y) in [(AlignItems::Start, 0.0), (AlignItems::End, 90.0)] {
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
    child_style.align_self = Some(align_self);

    let (_, y) = layout_abspos_child(container_style, child_style);
    assert!(
      (y - expected_y).abs() < 0.1,
      "expected y≈{expected_y} for align-self={align_self:?}, got {y}"
    );
  }
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
fn abspos_static_position_respects_wrap_reverse_with_horizontal_cross_axis_inside_content_box() {
  // Like `abspos_static_position_respects_wrap_reverse_with_horizontal_cross_axis`, but ensure the
  // wrap-reverse cross-axis flip is computed relative to the content box when the cross axis is
  // horizontal.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;

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
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  // Content box is 70×74. Under wrap-reverse, cross-start is content right: x = 2+5+(70-10)=67.
  assert!((x - 67.0).abs() < 0.1, "expected x≈67, got {}", x);
  assert!((y - 11.0).abs() < 0.1, "expected y≈11, got {}", y);
}

#[test]
fn abspos_static_position_respects_align_items_flex_end_under_wrap_reverse_with_horizontal_cross_axis() {
  // In a column flex container, the cross axis is horizontal. Under `wrap-reverse` the flex
  // cross-end edge becomes the physical left edge, so `align-items:flex-end` should place the child
  // at x≈0.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_respects_align_self_flex_start_under_wrap_reverse_with_horizontal_cross_axis() {
  // `align-self` should override `align-items` and still respect the wrap-reverse cross-start edge
  // for the `flex-start` keyword when the cross axis is horizontal.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::FlexEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.align_self = Some(AlignItems::FlexStart);

  let (x, y) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
}

#[test]
fn abspos_static_position_ignores_wrap_reverse_for_start_end_keywords_on_align_self_with_horizontal_cross_axis() {
  for (align_self, expected_x) in [(AlignItems::Start, 0.0), (AlignItems::End, 90.0)] {
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
    child_style.align_self = Some(align_self);

    let (x, _) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-self={align_self:?}, got {x}"
    );
  }
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
fn abspos_static_position_ignores_wrap_reverse_for_end_keyword_with_horizontal_cross_axis() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::End;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
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
fn abspos_static_position_respects_align_self_flex_end_under_wrap_reverse() {
  // Under `wrap-reverse`, `flex-end` aligns to the physical top edge (cross-end) for a horizontal
  // flex container, so abspos static-position probes must apply the same cross-axis mirroring.
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
  child_style.align_self = Some(AlignItems::FlexEnd);

  let (_, y) = layout_abspos_child(container_style, child_style);
  assert!((y - 0.0).abs() < 0.1, "expected y≈0, got {}", y);
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
fn abspos_static_position_respects_align_self_self_end_with_different_direction() {
  // Like the test above, but for `self-end`. With an RTL abspos child, the inline-end edge is
  // physical left, so `self-end` should align to x≈0.
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
  child_style.align_self = Some(AlignItems::SelfEnd);

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_respects_align_self_self_start_end_under_wrap_reverse() {
  // `self-start`/`self-end` align to the physical edge corresponding to the *item's* start/end
  // side, so they must not flip with `flex-wrap: wrap-reverse` (unlike `flex-start/flex-end`).
  //
  // Use an RTL abspos child so `self-start` maps to the physical right edge.
  for (align_items, align_self, expected_x) in [
    (AlignItems::FlexEnd, AlignItems::SelfStart, 90.0),
    (AlignItems::FlexStart, AlignItems::SelfEnd, 0.0),
  ] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.flex_direction = FlexDirection::Column;
    container_style.flex_wrap = FlexWrap::WrapReverse;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));
    child_style.direction = Direction::Rtl;
    child_style.align_self = Some(align_self);

    let (x, _) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?} align-self={align_self:?}, got {x}"
    );
  }
}

#[test]
fn abspos_static_position_respects_align_self_self_start_end_in_negative_cross_axis_writing_mode() {
  // Similar to the previous test, but in vertical writing mode the cross axis is the block axis and
  // can be negative (vertical-rl). Our flex adapter mirrors cross-axis coordinates in that case for
  // wrapping containers; ensure self-alignment still resolves against the child's own start/end.
  for (align_items, align_self, expected_x) in [
    (AlignItems::FlexStart, AlignItems::SelfStart, 0.0),
    (AlignItems::FlexEnd, AlignItems::SelfEnd, 90.0),
  ] {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Flex;
    container_style.position = Position::Relative;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(100.0));
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.flex_direction = FlexDirection::Row;
    container_style.flex_wrap = FlexWrap::Wrap;
    container_style.justify_content = JustifyContent::FlexStart;
    container_style.align_items = align_items;

    let mut child_style = ComputedStyle::default();
    child_style.position = Position::Absolute;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));
    child_style.direction = Direction::Ltr;
    child_style.align_self = Some(align_self);

    let (x, _) = layout_abspos_child(container_style, child_style);
    assert!(
      (x - expected_x).abs() < 0.1,
      "expected x≈{expected_x} for align-items={align_items:?} align-self={align_self:?}, got {x}"
    );
  }
}

#[test]
fn abspos_static_position_respects_align_items_self_start_with_different_direction() {
  // Same as above, but with `align-items: self-start` (so `align-self: auto` on the child inherits
  // a self-alignment value).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::SelfStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.direction = Direction::Rtl;

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_align_items_self_start_with_different_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::SelfStart;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.writing_mode = WritingMode::VerticalRl;

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 90.0).abs() < 0.1, "expected x≈90, got {}", x);
}

#[test]
fn abspos_static_position_respects_align_items_self_end_with_different_direction() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::SelfEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.direction = Direction::Rtl;

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
}

#[test]
fn abspos_static_position_respects_align_items_self_end_with_different_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.align_items = AlignItems::SelfEnd;

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Absolute;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.writing_mode = WritingMode::VerticalRl;

  let (x, _) = layout_abspos_child(container_style, child_style);
  assert!((x - 0.0).abs() < 0.1, "expected x≈0, got {}", x);
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
