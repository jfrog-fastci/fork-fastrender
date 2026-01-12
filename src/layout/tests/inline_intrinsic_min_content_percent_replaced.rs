use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{CrossOriginAttribute, ImageDecodingAttribute, ReplacedType};
use crate::{BoxNode, FormattingContextFactory, Size};
use std::sync::Arc;

#[test]
fn min_content_intrinsic_width_allows_percent_sized_replaced_to_shrink() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut block_style = ComputedStyle::default();
  block_style.display = Display::Block;

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Inline;
  img_style.width = Some(Length::percent(100.0));

  let img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Image {
      src: "example.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(1000.0, 500.0)),
    Some(2.0),
  );

  let para = BoxNode::new_block(
    Arc::new(block_style),
    FormattingContextType::Block,
    vec![img],
  );

  let min = ctx
    .compute_intrinsic_inline_size(&para, IntrinsicSizingMode::MinContent)
    .expect("min-content intrinsic size");

  assert!(
    min.abs() < 0.01,
    "expected percent-sized replaced element to have 0 min-content contribution (got {})",
    min
  );
}
