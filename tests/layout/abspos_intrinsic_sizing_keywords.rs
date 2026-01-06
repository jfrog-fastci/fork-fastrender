use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

fn positioned_root(children: Vec<BoxNode>) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.position = Position::Relative;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, children)
}

#[test]
fn abspos_width_max_content_shrinkwraps_text() {
  let fc = BlockFormattingContext::new();
  let text = "Hello world";

  let mut measure_style = ComputedStyle::default();
  measure_style.display = Display::Block;
  let measure = BoxNode::new_block(
    Arc::new(measure_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), text.into())],
  );
  let expected = fc
    .compute_intrinsic_inline_size(&measure, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic max-content width");

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = Some(Length::px(0.0));
  abs_style.top = Some(Length::px(0.0));
  abs_style.width = None;
  abs_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

  let abs_box = BoxNode::new_block(
    Arc::new(abs_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), text.into())],
  );
  let root = positioned_root(vec![abs_box]);
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(500.0, 200.0))
    .expect("layout");

  assert_eq!(fragment.children.len(), 1);
  assert_approx(
    fragment.children[0].bounds.width(),
    expected,
    "abspos width:max-content should shrinkwrap",
  );
}

#[test]
fn abspos_width_fit_content_clamps_to_available_between_insets() {
  let fc = BlockFormattingContext::new();
  let text = "lorem ipsum dolor sit amet";

  let mut measure_style = ComputedStyle::default();
  measure_style.display = Display::Block;
  let measure = BoxNode::new_block(
    Arc::new(measure_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), text.into())],
  );
  let (min_border, max_border) = fc
    .compute_intrinsic_inline_sizes(&measure)
    .expect("intrinsic inline sizes");
  assert!(
    max_border > min_border + 1.0,
    "expected max-content ({}) > min-content ({}) for fit-content test",
    max_border,
    min_border
  );

  let available = (min_border + max_border) / 2.0;
  assert!(available > min_border && available < max_border);

  let left = 10.0;
  let right = 10.0;
  let container_width = available + left + right;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = Some(Length::px(left));
  abs_style.right = Some(Length::px(right));
  abs_style.top = Some(Length::px(0.0));
  abs_style.width = None;
  abs_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

  let abs_box = BoxNode::new_block(
    Arc::new(abs_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), text.into())],
  );
  let root = positioned_root(vec![abs_box]);
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(container_width, 200.0))
    .expect("layout");

  assert_eq!(fragment.children.len(), 1);
  assert_approx(
    fragment.children[0].bounds.width(),
    available,
    "abspos width:fit-content should clamp to available size between insets",
  );
}

