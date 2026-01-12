use fastrender::style::display::Display;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextFactory;
use fastrender::FormattingContextType;
use std::sync::Arc;

#[test]
fn intrinsic_sizes_include_block_child_margins() {
  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(60.0));
  child_style.margin_left = Some(Length::px(8.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::InlineBlock;
  let wrapper = BoxNode::new_inline_block(
    Arc::new(wrapper_style),
    FormattingContextType::Block,
    vec![child],
  );

  let factory = FormattingContextFactory::new();
  let fc = factory.create(FormattingContextType::Block);
  let (min, max) = fc
    .compute_intrinsic_inline_sizes(&wrapper)
    .expect("intrinsic sizes for wrapper");

  let expected = 68.0;
  let eps = 0.5;
  assert!(
    (min - expected).abs() <= eps,
    "expected min-content intrinsic width to include the block child's margin; got {min}"
  );
  assert!(
    (max - expected).abs() <= eps,
    "expected max-content intrinsic width to include the block child's margin; got {max}"
  );
}
