use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::BoxNode;
use fastrender::Size;
use std::sync::Arc;

#[test]
fn flex_auto_min_size_percent_width_clamps_to_preferred_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(50.0));
  child_style.overflow_x = Overflow::Visible;
  child_style.overflow_y = Overflow::Visible;

  let text = BoxNode::new_text(Arc::new(ComputedStyle::default()), "X".repeat(200));
  let child_box = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![text]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_box],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    (width - 100.0).abs() < 0.5,
    "expected flex item percent width to clamp auto min-size (got {width})"
  );
}

#[test]
fn flex_auto_min_size_percent_width_clamps_even_when_container_width_auto() {
  // Container has width:auto but definite available width.
  // Add padding+border so the percent base must be the *content box* width:
  // - available border box width: 200
  // - padding L/R: 10, border L/R: 5 => content width base: 170
  // - child width 50% => 85
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_right = Length::px(10.0);
  container_style.border_left_width = Length::px(5.0);
  container_style.border_right_width = Length::px(5.0);
  container_style.border_left_style = BorderStyle::Solid;
  container_style.border_right_style = BorderStyle::Solid;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(50.0));
  child_style.overflow_x = Overflow::Visible;
  child_style.overflow_y = Overflow::Visible;

  let text = BoxNode::new_text(Arc::new(ComputedStyle::default()), "X".repeat(200));
  let child_box =
    BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![text]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_box],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    (width - 85.0).abs() < 0.5,
    "expected flex item percent width to clamp auto min-size using container content-box base (got {width})"
  );
}

#[test]
fn flex_auto_min_size_percent_height_clamps_even_when_container_height_auto() {
  // Column flex container has height:auto, but an outer layout algorithm (e.g. parent flex/grid)
  // resolved a definite used border-box height for it.
  // Add padding+border so the percent base must be the *content box* height:
  // - available border box height: 200
  // - padding T/B: 10, border T/B: 5 => content height base: 170
  // - child height 50% => 85
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.padding_top = Length::px(10.0);
  container_style.padding_bottom = Length::px(10.0);
  container_style.border_top_width = Length::px(5.0);
  container_style.border_bottom_width = Length::px(5.0);
  container_style.border_top_style = BorderStyle::Solid;
  container_style.border_bottom_style = BorderStyle::Solid;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::percent(50.0));
  child_style.overflow_x = Overflow::Visible;
  child_style.overflow_y = Overflow::Visible;

  let child_box = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(10.0, 1000.0)),
    None,
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_box],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite)
        .with_used_border_box_size(None, Some(200.0)),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let height = child.bounds.height();
  assert!(
    (height - 85.0).abs() < 0.5,
    "expected flex item percent height to clamp auto min-size using container content-box base (got {height})"
  );
}
