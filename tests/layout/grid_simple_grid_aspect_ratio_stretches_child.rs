use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignContent, AlignItems, AspectRatio, JustifyContent};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContextType};
use std::sync::Arc;

fn find_fragment_with_id<'a>(
  fragment: &'a fastrender::FragmentNode,
  id: usize,
) -> Option<&'a fastrender::FragmentNode> {
  if fragment.box_id().is_some_and(|fragment_id| fragment_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn assert_approx(value: f32, expected: f32, what: &str) {
  assert!(
    (value - expected).abs() < 0.5,
    "expected {what} to be {expected:.1}px (got {value:.1}px)",
  );
}

#[test]
fn grid_simple_grid_with_aspect_ratio_stretches_child_block_size() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(300.0));
  container_style.aspect_ratio = AspectRatio::Ratio(3.0 / 2.0);
  container_style.align_items = AlignItems::Stretch;
  container_style.justify_items = AlignItems::Stretch;
  container_style.align_content = AlignContent::Stretch;
  container_style.justify_content = JustifyContent::Normal;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 2;

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite_width(300.0))
    .expect("layout succeeds");

  assert_approx(fragment.bounds.height(), 200.0, "grid container height");
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(child_fragment.bounds.height(), 200.0, "stretched child height");
}

