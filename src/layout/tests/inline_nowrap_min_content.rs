use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::WhiteSpace;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(actual: f32, expected: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= 0.5,
    "{msg}: got {actual:.2} expected {expected:.2}",
  );
}

#[test]
fn nowrap_min_content_width_sums_inline_blocks() {
  // When an inline formatting context is `white-space: nowrap`, the min-content intrinsic width
  // should match the max-content intrinsic width (no soft wrapping opportunities). This matters
  // for flex/grid min-size calculations (`min-width:auto`).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.white_space = WhiteSpace::Nowrap;

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.white_space = WhiteSpace::Nowrap;
  let text_style = Arc::new(text_style);

  let mut inline_block_style = ComputedStyle::default();
  inline_block_style.display = Display::InlineBlock;
  inline_block_style.white_space = WhiteSpace::Nowrap;

  let inline_block_style = Arc::new(inline_block_style);
  let child_a = BoxNode::new_inline_block(
    inline_block_style.clone(),
    FormattingContextType::Block,
    vec![BoxNode::new_text(text_style.clone(), "Hello".to_string())],
  );
  let child_b = BoxNode::new_inline_block(
    inline_block_style,
    FormattingContextType::Block,
    vec![BoxNode::new_text(text_style, "World".to_string())],
  );

  let node = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![child_a, child_b],
  );

  let (min_content, max_content) = BlockFormattingContext::new()
    .compute_intrinsic_inline_sizes(&node)
    .expect("intrinsic inline sizes");

  assert!(
    max_content > 1.0,
    "expected max-content width to be non-zero; got {max_content:.2}"
  );
  assert_approx(
    min_content,
    max_content,
    "min-content should equal max-content under white-space:nowrap",
  );
}
