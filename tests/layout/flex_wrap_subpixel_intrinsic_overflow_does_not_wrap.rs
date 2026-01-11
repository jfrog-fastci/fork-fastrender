use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::JustifyContent;
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
  // Avoid flexing so wrapping decisions are driven by the measured intrinsic widths.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

fn shrinkable_block_style(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Allow flex-shrink so the line breaking decision is isolated to Taffy's
  // "would it fit" check rather than hard overflow.
  style.flex_shrink = 1.0;
  Arc::new(style)
}

fn github_like_cta_button(text: &str) -> BoxNode {
  let mut button_style = ComputedStyle::default();
  button_style.display = Display::Flex;
  button_style.flex_direction = FlexDirection::Row;
  button_style.flex_shrink = 0.0;
  // Match the GitHub CTA button padding from the page-loop snapshot.
  button_style.padding_left = Length::px(30.0);
  button_style.padding_right = Length::px(30.0);
  button_style.padding_top = Length::px(16.0);
  button_style.padding_bottom = Length::px(16.0);

  let mut inner_block_style = ComputedStyle::default();
  inner_block_style.display = Display::Block;
  // Avoid flexing inside the nested flex container.
  inner_block_style.flex_shrink = 0.0;

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;

  let mut text = BoxNode::new_text(Arc::new(text_style), text.to_string());
  text.id = 3;
  let mut inner = BoxNode::new_block(
    Arc::new(inner_block_style),
    FormattingContextType::Block,
    vec![text],
  );
  inner.id = 2;

  let mut button = BoxNode::new_block(
    Arc::new(button_style),
    FormattingContextType::Flex,
    vec![inner],
  );
  button.id = 1;
  button
}

#[test]
fn flex_wrap_gap_does_not_wrap_when_intrinsic_overflow_is_subpixel() {
  // Regresses: `FlexFormattingContext` can answer Taffy's intrinsic width probes with fractional
  // max-content sizes (from shaped text) even though later layout snaps used sizes to whole pixels.
  // Taffy line-breaking then wraps items that would fit after snapping (observed on github.com
  // `.CtaForm`).
  let fc = FlexFormattingContext::new();

  let left_width = 474.0;
  let gap = 16.0;

  let left = BoxNode::new_block(
    fixed_block_style(left_width, 56.0),
    FormattingContextType::Block,
    vec![],
  );
  let mut left = left;
  left.id = 10;

  let button = github_like_cta_button("Try GitHub Copilot free");
  let button_intrinsic = fc
    .compute_intrinsic_inline_size(&button, IntrinsicSizingMode::MaxContent)
    .expect("button max-content width");

  assert!(
    button_intrinsic > button_intrinsic.floor() + 0.1,
    "expected CTA button max-content width to include subpixel text advance; got {button_intrinsic}"
  );

  // Pick a container width that fits after pixel snapping (Taffy rounds to integer pixels when
  // producing its final layout). Without the fix, flex line-breaking can use unrounded values and
  // wrap even though the rounded layout would fit.
  let container_width = left_width + button_intrinsic.round() + gap;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::Center;
  container_style.grid_column_gap = Length::px(gap);
  container_style.grid_row_gap = Length::px(gap);
  container_style.width = Some(Length::px(container_width));
  container_style.width_keyword = None;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, button],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(container_width))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2, "expected two flex items");
  let second = &fragment.children[1];
  assert!(
    second.bounds.y().abs() <= 0.5,
    "expected CTA button to remain on the first line, got y={:.2} (container_width={container_width}, button_intrinsic={button_intrinsic})",
    second.bounds.y()
  );
}

#[test]
fn flex_wrap_does_not_wrap_when_definite_line_length_overflow_is_subpixel() {
  // Regresses: Taffy's flex line-breaking uses a strict `>` comparison against the available
  // main-axis space. When item sizes/available space are produced by floating point arithmetic, we
  // can end up with tiny (<1px) overflows that would disappear after pixel snapping, but still
  // trigger an unexpected wrap.
  let fc = FlexFormattingContext::new();

  let gap = 4.0;
  let item_width = 50.0;
  let container_width = item_width * 3.0 + gap * 2.0 - 0.25;

  let mut item_a = BoxNode::new_block(
    shrinkable_block_style(item_width, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  item_a.id = 10;

  let mut item_b = BoxNode::new_block(
    shrinkable_block_style(item_width, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  item_b.id = 11;

  let mut item_c = BoxNode::new_block(
    shrinkable_block_style(item_width, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  item_c.id = 12;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.grid_column_gap = Length::px(gap);
  container_style.grid_row_gap = Length::px(gap);
  container_style.width = Some(Length::px(container_width));
  container_style.width_keyword = None;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(container_width))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 3, "expected three flex items");
  let third = &fragment.children[2];
  assert!(
    third.bounds.y().abs() <= 0.5,
    "expected third item to remain on the first line, got y={:.2} (container_width={container_width})",
    third.bounds.y()
  );
}
