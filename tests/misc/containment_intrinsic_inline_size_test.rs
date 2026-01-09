use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::inline::InlineFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle};
use std::sync::Arc;

const EPSILON: f32 = 0.01;

fn expected_horizontal_edges() -> f32 {
  // Keep this in sync with the test styles below.
  // edges = padding-left + padding-right + border-left + border-right
  10.0 + 12.0 + 2.0 + 3.0
}

fn contain_layout_style(display: Display) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = display;
  style.containment.layout = true;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(12.0);
  style.border_left_width = Length::px(2.0);
  style.border_right_width = Length::px(3.0);
  Arc::new(style)
}

fn wide_text() -> BoxNode {
  let style = Arc::new(ComputedStyle::default());
  BoxNode::new_text(style, "W".repeat(200))
}

fn wide_inline_block() -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::InlineBlock;
  style.width = Some(Length::px(500.0));
  BoxNode::new_inline_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn contain_layout_isolates_intrinsic_width_in_block_fc() {
  let node = BoxNode::new_block(
    contain_layout_style(Display::Block),
    FormattingContextType::Block,
    vec![wide_text(), wide_inline_block()],
  );

  let (min, max) = BlockFormattingContext::new()
    .compute_intrinsic_inline_sizes(&node)
    .expect("compute intrinsic inline sizes");

  let edges = expected_horizontal_edges();
  assert!(
    (min - edges).abs() < EPSILON,
    "expected min-content intrinsic width to equal edges ({edges}), got {min}"
  );
  assert!(
    (max - edges).abs() < EPSILON,
    "expected max-content intrinsic width to equal edges ({edges}), got {max}"
  );
}

#[test]
fn contain_layout_isolates_intrinsic_width_in_inline_fc() {
  let node = BoxNode::new_inline(
    contain_layout_style(Display::Inline),
    vec![wide_text(), wide_inline_block()],
  );

  let (min, max) = InlineFormattingContext::new()
    .compute_intrinsic_inline_sizes(&node)
    .expect("compute intrinsic inline sizes");

  let edges = expected_horizontal_edges();
  assert!(
    (min - edges).abs() < EPSILON,
    "expected min-content intrinsic width to equal edges ({edges}), got {min}"
  );
  assert!(
    (max - edges).abs() < EPSILON,
    "expected max-content intrinsic width to equal edges ({edges}), got {max}"
  );
}
