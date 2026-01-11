use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::{CalcLength, Length, LengthUnit};
use fastrender::{BoxNode, BoxTree, ComputedStyle};

const EPS: f32 = 0.01;

fn assert_approx(a: f32, b: f32, msg: &str) {
  assert!(
    (a - b).abs() <= EPS,
    "{} (got {:.2}, expected {:.2})",
    msg,
    a,
    b
  );
}

#[test]
fn calc_percent_height_is_not_collapsible_through() {
  // Regression: an empty block with a non-zero used height computed from a percentage-based
  // `calc()` must not be treated as "collapsible-through" for margin collapsing. If it is, block
  // layout fails to advance the in-flow cursor and the next sibling overlaps it.
  //
  // This pattern appears on google.com, where a spacer uses `height: calc(100% - 200px)` above the
  // doodle image.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.height = Some(Length::px(230.0));
  container_style.height_keyword = None;

  let calc_height = CalcLength::single(LengthUnit::Percent, 100.0)
    .add_scaled(&CalcLength::single(LengthUnit::Px, -200.0), 1.0)
    .expect("calc height should be representable");
  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::calc(calc_height));
  spacer_style.height_keyword = None;

  let mut image_style = ComputedStyle::default();
  image_style.display = Display::Block;
  image_style.height = Some(Length::px(200.0));
  image_style.height_keyword = None;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]),
      BoxNode::new_block(Arc::new(image_style), FormattingContextType::Block, vec![]),
    ],
  );
  let tree = BoxTree::new(root);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Definite(400.0));
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let spacer_fragment = &fragment.children[0];
  let image_fragment = &fragment.children[1];

  assert_approx(
    spacer_fragment.bounds.height(),
    30.0,
    "expected spacer used height to resolve to 30px",
  );
  assert_approx(
    image_fragment.bounds.y(),
    30.0,
    "expected following block to be positioned after the spacer",
  );
}

