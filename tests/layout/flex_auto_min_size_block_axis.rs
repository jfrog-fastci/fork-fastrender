use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_auto_min_height_does_not_use_wrapped_min_content_block_size() {
  // Flexbox automatic minimum sizing (`min-height:auto`) uses a content-based minimum size.
  // When measuring intrinsic block sizes, `min-content` can produce a much taller block size than
  // `max-content` because narrower widths force more line wrapping.
  //
  // Ensure we don't treat the wrapped min-content block size as a hard minimum height for a flex
  // item, otherwise innocuous text can explode to viewport-sized heights (as seen on
  // theguardian.com cards).

  // Build a flex item containing many single-character words. The min-content inline size is the
  // width of "a", so a min-content block-size probe will wrap into ~N lines and become enormous,
  // while max-content can fit on one line and stays small.
  let text = "a ".repeat(200);
  let mut text_node = BoxNode::new_text(Arc::new(ComputedStyle::default()), text);
  text_node.id = 3;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  let mut item =
    BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![text_node]);
  item.id = 2;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(300.0));
  container_style.width_keyword = None;
  let mut container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![item]);
  container.id = 1;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let item_fragment = fragment.children.first().expect("flex item fragment");
  let height = item_fragment.bounds.height();
  assert!(
    height > 0.0 && height < 500.0,
    "expected flex item min-height:auto to avoid huge wrapped min-content block sizes (got {height})"
  );
}

