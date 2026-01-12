use std::sync::Arc;

use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::block::BlockFormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::ContainerType;
use crate::style::values::Length;
use crate::{BoxNode, BoxTree, ComputedStyle, FormattingContext};

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

fn layout_parent_with_container_type(container_type: ContainerType) -> (f32, f32) {
  let mut prev_style = ComputedStyle::default();
  prev_style.display = Display::Block;
  prev_style.height = Some(Length::px(0.0));
  let prev = BoxNode::new_block(Arc::new(prev_style), FormattingContextType::Block, vec![]);

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Block;
  outer_style.container_type = container_type;
  crate::style::properties::apply_container_type_implied_containment(&mut outer_style);
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![prev, outer],
  );

  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  (
    inner_fragment.bounds.y(),
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
  )
}

#[test]
fn container_type_size_and_inline_size_establish_bfc() {
  let (baseline_child_y, baseline_outer_gap) =
    layout_parent_with_container_type(ContainerType::Normal);
  assert_approx(
    baseline_child_y,
    0.0,
    "expected first-child margin to collapse out of the parent without container-type containment",
  );
  assert_approx(
    baseline_outer_gap,
    20.0,
    "expected collapsed margin to affect the parent's position among siblings",
  );

  for container_type in [
    ContainerType::Size,
    ContainerType::InlineSize,
    ContainerType::SizeScrollState,
    ContainerType::InlineSizeScrollState,
  ] {
    let (child_y, outer_gap) = layout_parent_with_container_type(container_type);
    assert_approx(
      child_y,
      20.0,
      "expected container-type to establish an independent formatting context (no parent/child margin collapse)",
    );
    assert_approx(
      outer_gap,
      0.0,
      "expected container-type containment to prevent the child's margin from escaping to siblings",
    );
  }
}
