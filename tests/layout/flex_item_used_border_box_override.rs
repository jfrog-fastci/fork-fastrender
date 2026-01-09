use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BoxSizing;
use fastrender::style::types::BorderStyle;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn style_with_display(display: Display) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = display;
  style
}

#[test]
fn flex_item_used_border_box_width_overrides_percent_width_during_block_layout() {
  // Regression for a flex+block integration bug:
  //
  // When flex/grid passes a `used_border_box_width` override to block layout (because Taffy has
  // already resolved the final item size), block layout must honor that override even if the
  // flex item has a non-auto authored width (e.g. `width: 50%`).
  //
  // Otherwise the percentage width can be applied twice: once by flex sizing to produce the used
  // flex item size, and then again by block layout resolving `width: 50%` against the already
  // sized item, causing descendants to be laid out in a narrower coordinate space than the final
  // fragment bounds.

  let mut container_style = style_with_display(Display::Flex);
  container_style.width = Some(Length::px(400.0));

  let mut item_style = style_with_display(Display::Block);
  item_style.box_sizing = BoxSizing::BorderBox;
  item_style.width = Some(Length::percent(50.0));
  item_style.border_left_width = Length::px(10.0);
  item_style.border_right_width = Length::px(10.0);
  item_style.border_left_style = BorderStyle::Solid;
  item_style.border_right_style = BorderStyle::Solid;

  let inner = BoxNode::new_block(
    Arc::new(style_with_display(Display::Block)),
    FormattingContextType::Block,
    vec![],
  );

  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(400.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let item_fragment = fragment.children.first().expect("flex item fragment");
  assert!(
    (item_fragment.bounds.width() - 200.0).abs() < 0.1,
    "expected flex item width 200px (50% of 400px), got {}",
    item_fragment.bounds.width()
  );

  let inner_fragment = item_fragment
    .children
    .first()
    .expect("inner block fragment");

  // With `box-sizing: border-box` and 10px borders on each side, the content box is 180px.
  assert!(
    (inner_fragment.bounds.width() - 180.0).abs() < 0.1,
    "inner block should fill flex item content box (expected 180px, got {})",
    inner_fragment.bounds.width()
  );
  assert!(
    (inner_fragment.bounds.x() - 10.0).abs() < 0.1,
    "inner block should be offset by border-left (expected x=10, got {})",
    inner_fragment.bounds.x()
  );
}
