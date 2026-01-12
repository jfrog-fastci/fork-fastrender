use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::FormattingContext;
use std::sync::Arc;

#[test]
fn flex_column_percent_padding_bottom_resolves_against_container_width_once() {
  // Regression test for the common "aspect ratio box" pattern:
  //
  //   .container { display:flex; flex-direction:column; width:200px }
  //   .container::after { content:""; display:block; padding-bottom:50% }
  //
  // On cnn.com this pattern is used to size media cards (e.g. 16:9 via 56.25%).
  // The pseudo-element is a flex item whose height should be its vertical padding.
  //
  // Previously our flex-item measurement path double-applied percentage padding, producing
  // a box height of 200px for `padding-bottom:50%` instead of the correct 100px.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = crate::style::types::FlexDirection::Column;
  container_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.padding_bottom = Length::percent(50.0);
  child_style.flex_shrink = 0.0;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  container.id = 100;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_frag = fragment.children.first().expect("child fragment");
  assert!(
    (child_frag.bounds.height() - 100.0).abs() < 0.5,
    "expected flex item height≈100 from padding-bottom:50%, got {}",
    child_frag.bounds.height()
  );
  assert!(
    (fragment.bounds.height() - 100.0).abs() < 0.5,
    "expected container height≈100 from single flex item, got {}",
    fragment.bounds.height()
  );
}
