use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexDirection;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn percent_height_computes_to_auto_without_definite_block_percentage_base() {
  // CSS2.1 §10.5: percentage heights compute to `auto` when the containing block height is not
  // definite. Layout code can still run with a definite available height (e.g. viewport/probes),
  // but that must not be used as the percentage base.
  //
  // Regression: FlexFormattingContext used `constraints.height()` as the percentage base, so a
  // `height:100%` flex container could incorrectly expand to the available height even when the
  // containing block height was not definite.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::percent(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
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
      &LayoutConstraints::new(
        AvailableSpace::Definite(100.0),
        AvailableSpace::Definite(100.0),
      ),
    )
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` without a definite percentage base (got {})",
    fragment.bounds.height()
  );
}

#[test]
fn percent_height_resolves_with_definite_block_percentage_base() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::percent(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(100.0),
    AvailableSpace::Definite(100.0),
  )
  .with_block_percentage_base(Some(100.0));

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 100.0).abs() < 0.5,
    "expected `height:100%` to resolve against the definite percentage base (got {})",
    fragment.bounds.height()
  );
}
