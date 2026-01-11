use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn abspos_child_origin_is_relative_to_padding_box_not_content_box() {
  // Regression test for positioned descendants in a padded block formatting context.
  //
  // When a block has non-zero padding, its in-flow children are translated into the fragment's
  // border-box coordinate space. Out-of-flow positioned children must receive the same
  // translation; otherwise `top: 0; left: 0` is offset into negative coordinates and can paint
  // outside the containing block (e.g. the IETF jumbotron ::before overlay darkening the header).

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(150.0));
  container_style.padding_left = Length::px(32.0);
  container_style.padding_top = Length::px(64.0);
  container_style.padding_right = Length::px(16.0);
  container_style.padding_bottom = Length::px(8.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("layout should succeed");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute positioned fragment present");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1,
    "expected abspos child x to align with the padding edge (got x = {})",
    abs_fragment.bounds.x()
  );
  assert!(
    abs_fragment.bounds.y().abs() < 0.1,
    "expected abspos child y to align with the padding edge (got y = {})",
    abs_fragment.bounds.y()
  );
}
