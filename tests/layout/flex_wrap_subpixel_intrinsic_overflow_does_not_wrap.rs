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

  // Pick a container width that exactly fits the *snapped* button size plus the fixed left item.
  // Without the fix, the intrinsic probe sees the fractional max-content size and wraps.
  let container_width = left_width + button_intrinsic.floor() + gap;

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
