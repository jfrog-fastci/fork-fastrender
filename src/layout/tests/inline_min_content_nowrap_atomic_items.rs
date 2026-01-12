use crate::style::display::Display;
use crate::style::types::WhiteSpace;
use crate::tree::box_tree::ReplacedType;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextFactory;
use crate::FormattingContextType;
use crate::IntrinsicSizingMode;
use crate::Size;
use std::sync::Arc;

#[test]
fn intrinsic_min_content_width_nowrap_sums_inline_blocks() {
  let mk_inline_block = |width: f32| {
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Block;
    let replaced = BoxNode::new_replaced(
      Arc::new(replaced_style),
      ReplacedType::Canvas,
      Some(Size::new(width, 10.0)),
      None,
    );

    let mut style = ComputedStyle::default();
    style.display = Display::InlineBlock;
    BoxNode::new_inline_block(
      Arc::new(style),
      FormattingContextType::Block,
      vec![replaced],
    )
  };

  let mk_container = |white_space: WhiteSpace| {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.white_space = white_space;
    let style = Arc::new(style);

    let a = mk_inline_block(40.0);
    let b = mk_inline_block(60.0);

    BoxNode::new_block(style, FormattingContextType::Block, vec![a, b])
  };

  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let normal_container = mk_container(WhiteSpace::Normal);
  let normal_min = ctx
    .compute_intrinsic_inline_size(&normal_container, IntrinsicSizingMode::MinContent)
    .expect("min-content intrinsic size for normal wrapping");
  let normal_max = ctx
    .compute_intrinsic_inline_size(&normal_container, IntrinsicSizingMode::MaxContent)
    .expect("max-content intrinsic size for normal wrapping");
  assert!(
    normal_max > normal_min + 1.0,
    "expected normal white-space min-content to be smaller than max-content (min={normal_min}, max={normal_max})"
  );

  let nowrap_container = mk_container(WhiteSpace::Nowrap);
  let nowrap_min = ctx
    .compute_intrinsic_inline_size(&nowrap_container, IntrinsicSizingMode::MinContent)
    .expect("min-content intrinsic size for nowrap");
  let nowrap_max = ctx
    .compute_intrinsic_inline_size(&nowrap_container, IntrinsicSizingMode::MaxContent)
    .expect("max-content intrinsic size for nowrap");

  // We avoid text shaping by using replaced children with known intrinsic sizes.
  let expected: f32 = 40.0 + 60.0;
  let eps = 0.5;
  assert!(
    (nowrap_min - expected).abs() <= eps,
    "expected nowrap min-content to sum atomic inline widths; expected≈{expected}, got {nowrap_min} (eps={eps})"
  );
  assert!(
    (nowrap_max - expected).abs() <= eps,
    "expected nowrap max-content to sum atomic inline widths; expected≈{expected}, got {nowrap_max} (eps={eps})"
  );
  assert!(
    (nowrap_min - nowrap_max).abs() <= eps,
    "expected nowrap min-content to match max-content; min={nowrap_min} max={nowrap_max} (eps={eps})"
  );
}
