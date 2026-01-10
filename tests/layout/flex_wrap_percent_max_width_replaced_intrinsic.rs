use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn flex_wrap_does_not_trigger_from_percent_max_width_inline_replaced_intrinsic_min_content() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(200.0));

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.flex_grow = 1.0;
  item_style.flex_shrink = 1.0;
  item_style.flex_basis = FlexBasis::Length(Length::px(0.0));
  item_style.overflow_x = Overflow::Visible;
  item_style.overflow_y = Overflow::Visible;

  let mut replaced_style = ComputedStyle::default();
  replaced_style.display = Display::Inline;
  replaced_style.max_width = Some(Length::percent(95.0));
  replaced_style.overflow_x = Overflow::Visible;
  replaced_style.overflow_y = Overflow::Visible;

  let replaced = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(1000.0, 100.0)),
    None,
  );

  let left_item = BoxNode::new_block(
    Arc::new(item_style.clone()),
    FormattingContextType::Block,
    vec![replaced],
  );

  let right_item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left_item, right_item],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  assert_eq!(
    fragment.children.len(),
    2,
    "expected 2 flex items, got {}",
    fragment.children.len()
  );

  let second = fragment.children.get(1).expect("second flex item fragment");
  let y = second.bounds.y();
  let eps = 0.5;
  assert!(
    y.abs() <= eps,
    "expected flex items to stay on the first line (y≈0); got y={y}"
  );
}

