use crate::css::properties::parse_length;
use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_calc_percent_width_ignores_negative_calc_padding_in_percent_base() {
  // Padding cannot be negative; if a `calc()` expression resolves to a negative value it must clamp
  // to `0`. In particular, negative padding must not inflate the flex container's inferred
  // content-box width, which is used as the percentage base for resolving `calc(% + <length>)`
  // widths on flex items.

  let negative_padding = parse_length("calc(50% - 100px)").expect("parse padding calc");
  let child_width = parse_length("calc(50% + 0px)").expect("parse child width calc");

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(parse_length("100px").unwrap());
  container_style.padding_left = negative_padding;
  container_style.padding_right = negative_padding;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(child_width);

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.first().expect("child fragment");
  let width = child_fragment.bounds.width();
  assert!(
    (width - 50.0).abs() < 0.5,
    "expected child width calc(50% + 0px) to resolve against 100px (got {width})"
  );
}
