use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::{
  AlignItems, BorderStyle, BoxSizing, Direction, FlexDirection, FlexWrap, JustifyContent, WritingMode,
};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
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

fn layout_abspos_child(container_style: ComputedStyle, child_style: ComputedStyle) -> (f32, f32) {
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
  let constraints = LayoutConstraints::definite(100.0, 100.0);
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
