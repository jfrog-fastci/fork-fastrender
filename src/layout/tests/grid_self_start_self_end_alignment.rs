use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::AlignItems;
use crate::style::types::Direction;
use crate::style::types::GridTrack;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment_with_id<'a>(
  fragment: &'a crate::FragmentNode,
  id: usize,
) -> Option<&'a crate::FragmentNode> {
  if fragment
    .box_id()
    .is_some_and(|fragment_id| fragment_id == id)
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn layout_single_child(
  container_style: ComputedStyle,
  child_style: ComputedStyle,
) -> crate::FragmentNode {
  let fc = GridFormattingContext::new();

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 2;

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 1;

  fc.layout(&grid, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds")
}

fn base_container_style() -> ComputedStyle {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.direction = Direction::Ltr;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  container_style
}

fn base_child_style() -> ComputedStyle {
  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style
}

#[test]
fn grid_justify_items_self_start_resolves_against_item_direction() {
  let mut container_style = base_container_style();
  container_style.justify_items = AlignItems::SelfStart;

  let mut child_style = base_child_style();
  child_style.direction = Direction::Rtl;

  let fragment = layout_single_child(container_style, child_style);
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    90.0,
    "self-start should resolve against the item's inline-start edge (rtl => right)",
  );
}

#[test]
fn grid_justify_items_self_end_resolves_against_item_direction() {
  let mut container_style = base_container_style();
  container_style.justify_items = AlignItems::SelfEnd;

  let mut child_style = base_child_style();
  child_style.direction = Direction::Rtl;

  let fragment = layout_single_child(container_style, child_style);
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "self-end should resolve against the item's inline-end edge (rtl => left)",
  );
}

#[test]
fn grid_justify_self_self_start_overrides_container_start() {
  let mut container_style = base_container_style();
  container_style.justify_items = AlignItems::Start;

  let mut child_style = base_child_style();
  child_style.direction = Direction::Rtl;
  child_style.justify_self = Some(AlignItems::SelfStart);

  let fragment = layout_single_child(container_style, child_style);
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    90.0,
    "justify-self:self-start should resolve against the item's direction, even when justify-items is start",
  );
}

#[test]
fn grid_justify_items_self_start_resolves_against_item_writing_mode() {
  let mut container_style = base_container_style();
  container_style.writing_mode = WritingMode::HorizontalTb;
  container_style.justify_items = AlignItems::SelfStart;

  let mut child_style = base_child_style();
  child_style.writing_mode = WritingMode::VerticalRl;

  let fragment = layout_single_child(container_style, child_style);
  let child_fragment = find_fragment_with_id(&fragment, 2).expect("child fragment");
  assert_approx(
    child_fragment.bounds.x(),
    90.0,
    "in vertical-rl, the physical x axis corresponds to the item's block axis (right→left), so self-start should align to the right edge",
  );
}
