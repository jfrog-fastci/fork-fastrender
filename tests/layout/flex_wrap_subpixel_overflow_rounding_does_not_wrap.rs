use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn fixed_block_style(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Avoid flexing so wrapping decisions are driven by the authored sizes.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

#[test]
fn flex_wrap_does_not_wrap_when_subpixel_overflow_disappears_after_rounding() {
  // Regression: Taffy rounds final layouts to integer pixels by default. Flex line-breaking runs
  // on unrounded sizes, which can spuriously wrap when items only overflow by a subpixel amount
  // that will be rounded away in the final layout.
  let fc = FlexFormattingContext::new();

  let container_width = 100.0;
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(container_width));
  container_style.width_keyword = None;

  let mut left = BoxNode::new_block(
    fixed_block_style(50.2, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  left.id = 1;
  let mut right = BoxNode::new_block(
    fixed_block_style(50.2, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  right.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, right],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(container_width))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2, "expected two flex items");
  let second = &fragment.children[1];
  assert!(
    second.bounds.y().abs() <= 0.5,
    "expected second item to remain on the first line, got y={:.2}",
    second.bounds.y()
  );
}

