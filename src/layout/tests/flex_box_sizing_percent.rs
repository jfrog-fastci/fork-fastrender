use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::BoxSizing;
use fastrender::style::types::FlexBasis;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_percent_width_content_box_adds_padding_and_border() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(50.0));
  // content-box by default
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

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    (width - 130.0).abs() < 0.1,
    "expected 50% content box (100px) + 30px padding/border = 130px border box (got {width})"
  );
}

#[test]
fn flex_percent_width_border_box_includes_padding_and_border() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.box_sizing = BoxSizing::BorderBox;
  child_style.width = Some(Length::percent(50.0));
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

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    (width - 100.0).abs() < 0.1,
    "expected 50% border box width = 100px (got {width})"
  );
}

#[test]
fn flex_basis_percent_content_box_adds_padding_and_border() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.flex_basis = FlexBasis::Length(Length::percent(50.0));
  // content-box by default
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

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    (width - 130.0).abs() < 0.1,
    "expected 50% flex-basis content box (100px) + 30px padding/border = 130px border box (got {width})"
  );
}
