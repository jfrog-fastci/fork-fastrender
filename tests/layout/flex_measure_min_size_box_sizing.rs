use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::AlignItems;
use fastrender::style::types::BoxSizing;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_measure_min_height_border_box_does_not_double_apply_padding() {
  // Regression: when flex/grid measure a subtree via a nested formatting context, we convert the
  // resulting fragment border-box size back into the *content-box* size that Taffy expects from
  // the measure callback.
  //
  // When `box-sizing: border-box`, `min-height` applies to the border box (including
  // padding/border). The measure callback must not treat that `min-height` as a content-box
  // constraint, or padding will effectively be applied twice and inflate the flex item.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  // Force Taffy to provide a definite available cross size to the measure callback. This ensures
  // the measure code path resolves min/max sizes (including absolute lengths) instead of skipping
  // them due to a missing percentage base.
  container_style.height = Some(Length::px(200.0));
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Flex;
  child_style.box_sizing = BoxSizing::BorderBox;
  child_style.min_height = Some(Length::px(56.0));
  child_style.padding_top = Length::px(16.7);
  child_style.padding_bottom = Length::px(16.7);
  child_style.padding_left = Length::px(24.0);
  child_style.padding_right = Length::px(24.0);

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Flex, vec![]);
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
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(200.0),
      ),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.first().expect("flex child fragment");
  let height = child_fragment.bounds.height();
  assert!(
    (height - 56.0).abs() < 0.1,
    "expected border-box min-height (56px) to win even with padding under border-box sizing; got {height}"
  );
}
