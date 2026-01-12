use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AlignItems;
use crate::style::types::JustifyContent;
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::ComputedStyle;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn justify_content_normal_stretches_auto_tracks_in_grid() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(240.0));
  container_style.height = Some(Length::px(120.0));
  container_style.align_items = AlignItems::Center;
  container_style.justify_items = AlignItems::Center;
  container_style.justify_content = JustifyContent::Normal;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  let child_style = Arc::new(child_style);

  let mut child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(240.0, 120.0))
    .expect("grid layout succeeds");

  let child_fragment = fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(1))
    .expect("child fragment");

  assert_approx(
    child_fragment.bounds.x(),
    115.0,
    "child should be centered along the inline axis",
  );
  assert_approx(
    child_fragment.bounds.y(),
    55.0,
    "child should be centered along the block axis",
  );
}
