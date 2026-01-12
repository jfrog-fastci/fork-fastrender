use crate::geometry::Size;
use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::{AlignItems, FlexDirection, LineHeight};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_auto_min_size_column_does_not_inflate_single_line_items() {
  let viewport = Size::new(500.0, 500.0);
  let fc = FlexFormattingContext::with_viewport(viewport);

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  // Match the common "column + center" pattern used for vertically centered empty/error states:
  // items shrink-to-fit in the cross axis rather than stretching to the container width.
  container_style.align_items = AlignItems::Center;

  let line_height = 10.0;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.font_size = line_height;
  child_style.root_font_size = line_height;
  child_style.line_height = LineHeight::Length(Length::px(line_height));

  let mut text_style = ComputedStyle::default();
  text_style.font_size = line_height;
  text_style.root_font_size = line_height;
  text_style.line_height = LineHeight::Length(Length::px(line_height));

  // At the min-content inline size (longest word), this string wraps to two lines. At the
  // max-content inline size, it fits on one line.
  let text = BoxNode::new_text(Arc::new(text_style), "Hello World".to_string());
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![text],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(400.0), AvailableSpace::Indefinite);
  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let child_fragment = &fragment.children[0];

  assert!(
    (child_fragment.bounds.height() - line_height).abs() < 0.5,
    "expected single-line flex item height ~{line_height}px, got {:.2}px",
    child_fragment.bounds.height()
  );
}
