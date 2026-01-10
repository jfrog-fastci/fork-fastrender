use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, BoxSizing, GridTrack};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContextType};
use std::sync::Arc;

fn find_fragment_with_id<'a>(fragment: &'a fastrender::FragmentNode, id: usize) -> Option<&'a fastrender::FragmentNode> {
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

fn layout_single_child(child_style: ComputedStyle) -> fastrender::FragmentNode {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(300.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  container_style.justify_items = AlignItems::Stretch;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 2;

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 1;

  fc.layout(&grid, &LayoutConstraints::definite(300.0, 20.0))
    .expect("layout succeeds")
}

fn assert_approx(value: f32, expected: f32, what: &str) {
  assert!(
    (value - expected).abs() < 0.5,
    "expected {what} to be {expected:.1}px (got {value:.1}px)",
  );
}

fn assert_grid_item_max_width_case(style: ComputedStyle, expected_border_box_width: f32) {
  let fragment = layout_single_child(style);
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(
    child_fragment.bounds.width(),
    expected_border_box_width,
    "stretched grid item border-box width",
  );
}

#[test]
fn grid_item_auto_width_stretch_respects_max_width() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = None;
  style.width_keyword = None;
  style.max_width = Some(Length::px(150.0));
  style.max_width_keyword = None;
  style.height = Some(Length::px(10.0));
  style.height_keyword = None;
  style.justify_self = Some(AlignItems::Stretch);
  assert_grid_item_max_width_case(style, /* expected_border_box_width */ 150.0);
}

#[test]
fn grid_item_auto_width_stretch_respects_max_width_with_padding_content_box() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = None;
  style.width_keyword = None;
  style.max_width = Some(Length::px(150.0));
  style.max_width_keyword = None;
  style.height = Some(Length::px(10.0));
  style.height_keyword = None;
  style.justify_self = Some(AlignItems::Stretch);
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 150px constrains the *content* width under content-box sizing.
  assert_grid_item_max_width_case(style, /* expected_border_box_width */ 170.0);
}

#[test]
fn grid_item_auto_width_stretch_respects_max_width_with_padding_border_box() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = None;
  style.width_keyword = None;
  style.max_width = Some(Length::px(150.0));
  style.max_width_keyword = None;
  style.height = Some(Length::px(10.0));
  style.height_keyword = None;
  style.justify_self = Some(AlignItems::Stretch);
  style.box_sizing = BoxSizing::BorderBox;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 150px constrains the border box under border-box sizing.
  assert_grid_item_max_width_case(style, /* expected_border_box_width */ 150.0);
}

