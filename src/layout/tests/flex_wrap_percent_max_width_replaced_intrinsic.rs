use crate::geometry::Size;
use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexBasis;
use crate::style::types::FlexWrap;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::ReplacedType;
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

  let right_item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

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
