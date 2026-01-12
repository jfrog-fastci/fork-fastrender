use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContextType};
use std::sync::Arc;

#[test]
fn intrinsic_min_content_does_not_break_after_nbsp_before_inline_block() {
  // Regression test: a trailing NBSP should glue the following inline-block to the preceding text,
  // preventing intrinsic min-content sizing from treating them as separate segments.
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let text = BoxNode::new_text(
    Arc::new(ComputedStyle::default()),
    "Community\u{00A0}".to_string(),
  );

  let mut inline_block_style = ComputedStyle::default();
  inline_block_style.display = Display::InlineBlock;
  inline_block_style.width = Some(Length::px(20.0));
  let inline_block = BoxNode::new_inline_block(
    Arc::new(inline_block_style),
    FormattingContextType::Block,
    vec![],
  );

  let node = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![text, inline_block],
  );

  let (min_content, max_content) = BlockFormattingContext::new()
    .compute_intrinsic_inline_sizes(&node)
    .expect("intrinsic inline sizes");

  assert!(
    (min_content - max_content).abs() <= 0.5,
    "NBSP should prevent breaking between the text and following inline-block; expected min-content ({min_content:.2}) ~= max-content ({max_content:.2})"
  );
}
