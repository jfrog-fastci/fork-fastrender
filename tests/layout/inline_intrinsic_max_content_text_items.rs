use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::types::FontWeight;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextFactory;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn block_container(children: Vec<BoxNode>) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, children)
}

#[test]
fn max_content_width_sums_across_adjacent_text_items() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut style_a = ComputedStyle::default();
  style_a.display = Display::Inline;
  style_a.font_weight = FontWeight::Normal;

  let mut style_b = style_a.clone();
  style_b.font_weight = FontWeight::Bold;

  let combined = block_container(vec![
    BoxNode::new_text(Arc::new(style_a), "Hello".into()),
    BoxNode::new_text(Arc::new(style_b), "World".into()),
  ]);
  let combined_width = ctx
    .compute_intrinsic_inline_size(&combined, IntrinsicSizingMode::MaxContent)
    .expect("combined max-content width");

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Inline;
  first_style.font_weight = FontWeight::Normal;
  let first = block_container(vec![BoxNode::new_text(Arc::new(first_style), "Hello".into())]);
  let first_width = ctx
    .compute_intrinsic_inline_size(&first, IntrinsicSizingMode::MaxContent)
    .expect("first max-content width");

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Inline;
  second_style.font_weight = FontWeight::Bold;
  let second = block_container(vec![BoxNode::new_text(Arc::new(second_style), "World".into())]);
  let second_width = ctx
    .compute_intrinsic_inline_size(&second, IntrinsicSizingMode::MaxContent)
    .expect("second max-content width");

  let expected = first_width + second_width;
  assert!(
    (combined_width - expected).abs() <= 0.5,
    "expected combined max-content width to sum across adjacent text items; got {combined_width}, expected {expected} (first={first_width}, second={second_width})"
  );
}
