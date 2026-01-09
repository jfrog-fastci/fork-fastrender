use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextFactory;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

#[test]
fn max_content_intrinsic_width_sums_across_inline_items() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut block_style = ComputedStyle::default();
  block_style.display = Display::Block;

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;

  // Ensure inline content is split across multiple text items (text node + inline element)
  // while still forming a single line in max-content sizing.
  let para = BoxNode::new_block(
    Arc::new(block_style),
    FormattingContextType::Block,
    vec![
      BoxNode::new_text(default_style(), "Hello ".into()),
      BoxNode::new_inline(
        Arc::new(inline_style),
        vec![BoxNode::new_text(default_style(), "world".into())],
      ),
    ],
  );

  let min = ctx
    .compute_intrinsic_inline_size(&para, IntrinsicSizingMode::MinContent)
    .expect("min-content intrinsic size");
  let max = ctx
    .compute_intrinsic_inline_size(&para, IntrinsicSizingMode::MaxContent)
    .expect("max-content intrinsic size");

  assert!(
    max > min + 1.0,
    "expected max-content ({}) > min-content ({}) when inline content spans multiple items",
    max,
    min
  );
}

