use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{AspectRatio, InsetValue};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

#[test]
fn abspos_auto_auto_aspect_ratio_does_not_use_in_flow_width_for_non_replaced() {
  // Regression for gitlab.com: absolutely positioned, non-replaced elements with `aspect-ratio`
  // and `width/height:auto` must not treat an earlier in-flow layout pass as an "intrinsic size".
  // In particular, block-level boxes (e.g. `display:flex`) may have expanded to the containing
  // block width during that measurement, which would incorrectly turn `aspect-ratio: 1/1` into a
  // huge square and push centered content off-screen.

  let fc = BlockFormattingContext::new();

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.position = Position::Relative;
  root_style.width = Some(Length::px(200.0));
  root_style.height = Some(Length::px(200.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Flex;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.aspect_ratio = AspectRatio::Ratio(1.0);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(50.0));
  child_style.height = Some(Length::px(50.0));

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let abs_box = BoxNode::new_block(
    Arc::new(abs_style),
    FormattingContextType::Flex,
    vec![child],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![abs_box],
  );

  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment");

  assert_approx(
    abs_fragment.bounds.width(),
    50.0,
    "abspos auto width should shrink-to-fit content",
  );
  assert_approx(
    abs_fragment.bounds.height(),
    50.0,
    "abspos auto height should shrink-to-fit content",
  );
}
