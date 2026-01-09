use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextFactory;
use fastrender::FormattingContextType;
use std::sync::Arc;

#[test]
fn inline_block_intrinsic_sizes_include_float_children() {
  let mut float_style = ComputedStyle::default();
  float_style.display = Display::Block;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(10.0));
  let float_style = Arc::new(float_style);

  let float_1 = BoxNode::new_block(float_style.clone(), FormattingContextType::Block, vec![]);
  let float_2 = BoxNode::new_block(float_style, FormattingContextType::Block, vec![]);

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::InlineBlock;
  let wrapper = BoxNode::new_inline_block(
    Arc::new(wrapper_style),
    FormattingContextType::Block,
    vec![float_1, float_2],
  );

  let factory = FormattingContextFactory::new();
  let fc = factory.create(FormattingContextType::Block);
  let (min, max) = fc
    .compute_intrinsic_inline_sizes(&wrapper)
    .expect("intrinsic sizes for inline-block");

  assert!(
    min >= 49.9,
    "min-content intrinsic width should include the widest float child, got {min}"
  );
  assert!(
    max >= 99.9,
    "max-content intrinsic width should include the sum of float children, got {max}"
  );
}
