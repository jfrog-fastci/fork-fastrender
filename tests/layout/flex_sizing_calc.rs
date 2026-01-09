use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AlignItems;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::BoxSizing;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn flex_item_width_calc_percent_resolves_against_container_inner_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(calc_percent_plus_px(50.0, 10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  assert!(
    (child_fragment.bounds.width() - 110.0).abs() < 0.5,
    "expected child width ≈ 110px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_max_width_calc_percent_clamps_border_box_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(Length::px(200.0));
  child_style.max_width = Some(calc_percent_plus_px(50.0, 10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  assert!(
    (child_fragment.bounds.width() - 110.0).abs() < 0.5,
    "expected child width clamped to ≈ 110px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_width_calc_percent_uses_container_content_box_base() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  // Ensure percentage bases use the container's *inner* (content-box) width, not the border box.
  container_style.padding_left = Length::px(10.0);
  container_style.padding_right = Length::px(10.0);
  container_style.border_left_width = Length::px(5.0);
  container_style.border_right_width = Length::px(5.0);
  container_style.border_left_style = BorderStyle::Solid;
  container_style.border_right_style = BorderStyle::Solid;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(calc_percent_plus_px(50.0, 10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  // Container content-box width is 200 - (10+10 padding) - (5+5 border) = 170.
  // calc(50% + 10px) => 0.5*170 + 10 = 95px.
  assert!(
    (child_fragment.bounds.width() - 95.0).abs() < 0.5,
    "expected child width ≈ 95px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_content_box_calc_percent_width_includes_padding_and_border() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.box_sizing = BoxSizing::ContentBox;
  child_style.width = Some(calc_percent_plus_px(50.0, 10.0));
  child_style.padding_left = Length::px(10.0);
  child_style.padding_right = Length::px(10.0);
  child_style.border_left_width = Length::px(5.0);
  child_style.border_right_width = Length::px(5.0);
  child_style.border_left_style = BorderStyle::Solid;
  child_style.border_right_style = BorderStyle::Solid;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  // width is content-box: calc(50% + 10px) => 0.5*200 + 10 = 110 content px.
  // Add 10+10 padding + 5+5 border = 30 => border-box width 140.
  assert!(
    (child_fragment.bounds.width() - 140.0).abs() < 0.5,
    "expected child border-box width ≈ 140px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_border_box_calc_percent_width_excludes_padding_and_border() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.box_sizing = BoxSizing::BorderBox;
  child_style.width = Some(calc_percent_plus_px(50.0, 10.0));
  child_style.padding_left = Length::px(10.0);
  child_style.padding_right = Length::px(10.0);
  child_style.border_left_width = Length::px(5.0);
  child_style.border_right_width = Length::px(5.0);
  child_style.border_left_style = BorderStyle::Solid;
  child_style.border_right_style = BorderStyle::Solid;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  // width is border-box: calc(50% + 10px) => 0.5*200 + 10 = 110 border-box px.
  assert!(
    (child_fragment.bounds.width() - 110.0).abs() < 0.5,
    "expected child border-box width ≈ 110px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_height_calc_percent_resolves_against_container_inner_height() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.align_items = AlignItems::FlexStart;
  // Force a definite container border-box height via `box-sizing: border-box`.
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.height = Some(Length::px(200.0));
  container_style.padding_top = Length::px(20.0);
  container_style.padding_bottom = Length::px(20.0);
  container_style.border_top_width = Length::px(5.0);
  container_style.border_bottom_width = Length::px(5.0);
  container_style.border_top_style = BorderStyle::Solid;
  container_style.border_bottom_style = BorderStyle::Solid;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.height = Some(calc_percent_plus_px(50.0, 10.0));
  child_style.width = Some(Length::px(10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  // Simulate block layout passing the resolved used border-box height.
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite)
    .with_used_border_box_size(Some(200.0), Some(200.0));

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  // Container content-box height is 200 - (20+20 padding) - (5+5 border) = 150.
  // calc(50% + 10px) => 0.5*150 + 10 = 85px.
  assert!(
    (child_fragment.bounds.height() - 85.0).abs() < 0.5,
    "expected child height ≈ 85px, got {}",
    child_fragment.bounds.height()
  );
}
