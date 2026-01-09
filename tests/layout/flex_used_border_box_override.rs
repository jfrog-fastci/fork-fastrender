use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext};

const EPS: f32 = 0.01;

fn assert_approx(got: f32, expected: f32, msg: &str) {
  assert!(
    (got - expected).abs() <= EPS,
    "{} (got {:.2}, expected {:.2})",
    msg,
    got,
    expected
  );
}

#[test]
fn flex_item_used_border_box_width_override_applies_when_width_is_percentage() {
  // Regression for block layout treating `constraints.used_border_box_width` as advisory only for
  // `width:auto`. Flex/grid layout can resolve an item's final border-box size regardless of the
  // authored `width` (including percentages), so block layout must honor the override to ensure
  // children reflow within the resolved size.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(500.0));
  container_style.width_keyword = None;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::percent(50.0));
  item_style.width_keyword = None;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(10.0));
  inner_style.height_keyword = None;

  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![inner]);
  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![item]);

  let fragment = FlexFormattingContext::new()
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Indefinite),
    )
    .expect("layout");

  let item_fragment = &fragment.children[0];
  let inner_fragment = &item_fragment.children[0];

  assert_approx(
    item_fragment.bounds.width(),
    250.0,
    "expected flex item border-box width to resolve to 50% of the flex container",
  );
  assert_approx(
    inner_fragment.bounds.width(),
    250.0,
    "expected block children to reflow within the flex-resolved border-box width",
  );
}

