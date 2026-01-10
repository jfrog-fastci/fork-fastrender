use fastrender::css::properties::parse_length;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::{FormattingContext, IntrinsicSizingMode};
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn block_intrinsic_width_includes_block_child_margins() {
  // A block container's intrinsic inline sizes are based on the *outer* size of its in-flow
  // children. In particular, horizontal margins on block-level children contribute to the parent's
  // min/max-content widths. This is relied upon by content-sized flex items whose width is derived
  // from their children.

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(parse_length("28px").unwrap());
  child_style.margin_left = Some(parse_length("5px").unwrap());
  child_style.margin_right = Some(parse_length("9px").unwrap());

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  let parent = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Block,
    vec![child],
  );

  let bfc = BlockFormattingContext::new();
  let max = bfc
    .compute_intrinsic_inline_size(&parent, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic sizing should succeed");

  assert!(
    (max - 42.0).abs() < 0.5,
    "expected block child margins to contribute to intrinsic width (got {max})"
  );
}

