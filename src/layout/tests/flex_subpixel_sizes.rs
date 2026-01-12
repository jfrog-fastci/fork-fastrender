use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use std::sync::Arc;

#[test]
fn flex_layout_preserves_subpixel_sizes() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(19.4));

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.height() - 19.4).abs() < 1e-3,
    "expected flex container height to preserve subpixel length (got {})",
    fragment.bounds.height()
  );
}
