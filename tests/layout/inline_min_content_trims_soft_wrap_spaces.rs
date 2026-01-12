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
    (actual - expected).abs() <= 1.0,
    "{msg}: got {actual:.2} expected {expected:.2}",
  );
}

#[test]
fn inline_min_content_trims_soft_wrap_trailing_spaces() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  let container_style = Arc::new(container_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.white_space = WhiteSpace::Normal;
  text_style.word_spacing = 100.0;
  text_style.font_family = Arc::from(vec!["Noto Sans Mono".to_string()]);
  text_style.font_size = 20.0;
  let text_style = Arc::new(text_style);

  let mk_block = |text: &str| {
    BoxNode::new_block(
      container_style.clone(),
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style.clone(), text.to_string())],
    )
  };

  let (phrase_min, _phrase_max) = BlockFormattingContext::new()
    .compute_intrinsic_inline_sizes(&mk_block("infrastructure to"))
    .expect("intrinsic inline sizes for phrase");
  let (_infra_min, infra_max) = BlockFormattingContext::new()
    .compute_intrinsic_inline_sizes(&mk_block("infrastructure"))
    .expect("intrinsic inline sizes for word");

  assert_approx(
    phrase_min,
    infra_max,
    "min-content should not include trailing collapsible spaces at soft wrap boundaries",
  );
}
