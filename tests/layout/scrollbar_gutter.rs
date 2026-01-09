use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::utils::resolve_scrollbar_width;
use fastrender::style::types::Overflow;
use fastrender::style::types::ScrollbarGutter;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn layout_with_container(style: ComputedStyle) -> (f32, f32) {
  let gutter = resolve_scrollbar_width(&style);
  let container_style = Arc::new(style);

  let mut child_style = ComputedStyle::default();
  child_style.width = Some(Length::percent(100.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(container_style, FormattingContextType::Block, vec![child]);

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  (fragment.children[0].bounds.width(), gutter)
}

#[test]
fn scrollbar_gutter_stable_reserves_inline_end_space() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Auto;
  style.scrollbar_gutter = ScrollbarGutter {
    stable: true,
    both_edges: false,
  };

  let (child_width, gutter) = layout_with_container(style);
  assert!((child_width - (100.0 - gutter)).abs() < 1e-3);
}

#[test]
fn scrollbar_gutter_stable_both_edges_reserves_space_on_both_sides() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Auto;
  style.scrollbar_gutter = ScrollbarGutter {
    stable: true,
    both_edges: true,
  };

  let (child_width, gutter) = layout_with_container(style);
  assert!((child_width - (100.0 - gutter * 2.0)).abs() < 1e-3);
}

#[test]
fn scrollbar_gutter_auto_does_not_reserve_space_without_scroll() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Auto;

  let (child_width, gutter) = layout_with_container(style);
  assert_eq!(gutter, resolve_scrollbar_width(&ComputedStyle::default()));
  assert!((child_width - 100.0).abs() < 1e-3);
}

#[test]
fn scrollbar_gutter_stable_reserves_space_for_overflow_hidden() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Hidden;
  style.scrollbar_gutter = ScrollbarGutter {
    stable: true,
    both_edges: false,
  };

  let (child_width, gutter) = layout_with_container(style);
  assert!((child_width - (100.0 - gutter)).abs() < 1e-3);
}

#[test]
fn scrollbar_gutter_auto_does_not_reserve_for_overflow_hidden() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Hidden;

  let (child_width, _gutter) = layout_with_container(style);
  assert!((child_width - 100.0).abs() < 1e-3);
}

#[test]
fn scrollbar_auto_does_not_reserve_space_when_overflowing_by_default() {
  fn layout_overflowing_container(style: ComputedStyle) -> (f32, f32) {
    let gutter = resolve_scrollbar_width(&style);
    let container_style = Arc::new(style);

    let mut tall_style = ComputedStyle::default();
    tall_style.height = Some(Length::px(200.0));
    let tall = BoxNode::new_block(Arc::new(tall_style), FormattingContextType::Block, vec![]);

    let mut probe_style = ComputedStyle::default();
    probe_style.width = Some(Length::percent(100.0));
    let probe = BoxNode::new_block(Arc::new(probe_style), FormattingContextType::Block, vec![]);

    let container =
      BoxNode::new_block(container_style, FormattingContextType::Block, vec![tall, probe]);

    let bfc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(100.0, 50.0);
    let fragment = bfc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(fragment.children.len(), 2);
    (fragment.children[1].bounds.width(), gutter)
  }

  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Auto;
  style.height = Some(Length::px(50.0));

  let (child_width, _gutter) = layout_overflowing_container(style.clone());
  // Scrollbars are modeled as overlay by default, so overflowing `overflow-y:auto` should not
  // reserve layout space unless `scrollbar-gutter: stable` is set.
  assert!((child_width - 100.0).abs() < 1e-3);

  style.scrollbar_gutter = ScrollbarGutter {
    stable: true,
    both_edges: false,
  };
  let (child_width, gutter) = layout_overflowing_container(style);
  assert!((child_width - (100.0 - gutter)).abs() < 1e-3);
}

#[test]
fn scrollbar_auto_cross_axis_overflow_does_not_reserve_space_by_default() {
  let mut style = ComputedStyle::default();
  style.overflow_x = Overflow::Auto;
  style.overflow_y = Overflow::Auto;
  style.height = Some(Length::px(50.0));

  let mut wide_tall_style = ComputedStyle::default();
  wide_tall_style.width = Some(Length::px(100.0));
  wide_tall_style.height = Some(Length::px(200.0));
  let wide_tall = BoxNode::new_block(
    Arc::new(wide_tall_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut probe_style = ComputedStyle::default();
  probe_style.width = Some(Length::percent(100.0));
  let probe = BoxNode::new_block(Arc::new(probe_style), FormattingContextType::Block, vec![]);

  let container =
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![wide_tall, probe]);
  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!((fragment.scrollbar_reservation.right - 0.0).abs() < 1e-3);
  assert!((fragment.scrollbar_reservation.bottom - 0.0).abs() < 1e-3);
  assert!((fragment.children[1].bounds.width() - 100.0).abs() < 1e-3);
}

#[test]
fn overflow_x_scroll_does_not_inflate_fixed_height() {
  let mut style = ComputedStyle::default();
  style.overflow_x = Overflow::Scroll;
  style.height = Some(Length::px(100.0));

  let container = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  // Scrollbar gutters reserve space inside the scrollport without changing the outer border box
  // size. Previously, adding a horizontal scrollbar gutter via overflow-x would effectively
  // increase padding and inflate the border box for content-box definite heights.
  assert!((fragment.bounds.height() - 100.0).abs() < 1e-3);
}

#[test]
fn overflow_x_scroll_does_not_inflate_fixed_height_with_min_height() {
  let mut style = ComputedStyle::default();
  style.overflow_x = Overflow::Scroll;
  style.height = Some(Length::px(100.0));
  style.min_height = Some(Length::px(100.0));

  let container = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!((fragment.bounds.height() - 100.0).abs() < 1e-3);
}

#[test]
fn overflow_y_scroll_does_not_inflate_fixed_width_with_min_width() {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Scroll;
  style.width = Some(Length::px(100.0));
  style.min_width = Some(Length::px(100.0));

  let container = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!((fragment.bounds.width() - 100.0).abs() < 1e-3);
}
