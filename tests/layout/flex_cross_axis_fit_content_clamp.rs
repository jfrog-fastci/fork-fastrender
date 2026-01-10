use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{AlignItems, FlexDirection, LineHeight};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_cross_axis_auto_clamps_to_available_when_max_content_exceeds_container() {
  let viewport = Size::new(500.0, 500.0);
  let fc = FlexFormattingContext::with_viewport(viewport);

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  // Common "column + center" pattern: items are not stretched in the cross axis.
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

  // Max-content is wide, but min-content (longest word) is narrow: fit-content should clamp to
  // the available cross size and wrap.
  let text = BoxNode::new_text(
    Arc::new(text_style),
    "This is a very long piece of text that should wrap across multiple lines".to_string(),
  );
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![text]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = fc.layout(&container, &constraints).expect("layout should succeed");

  assert_eq!(fragment.children.len(), 1);
  let child_fragment = &fragment.children[0];

  assert!(
    (child_fragment.bounds.width() - 200.0).abs() < 0.6,
    "expected flex item cross size to clamp to 200px, got {:.2}px",
    child_fragment.bounds.width()
  );
  assert!(
    child_fragment.bounds.height() > line_height * 1.5,
    "expected wrapped flex item height > {:.2}px, got {:.2}px",
    line_height * 1.5,
    child_fragment.bounds.height()
  );
}

