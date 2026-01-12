use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::style::display::Display;
use crate::style::types::AlignItems;
use crate::style::types::GridTrack;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
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

fn find_first_fragment_with_id<'a>(
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
    if let Some(found) = find_first_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn vertical_writing_mode_grid_inside_block_is_not_double_transformed() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(200.0));
  root_style.height = Some(Length::px(200.0));
  let root_style = Arc::new(root_style);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.writing_mode = WritingMode::VerticalLr;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(50.0));
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
  let grid_style = Arc::new(grid_style);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(20.0));
  child_style.height = Some(Length::px(10.0));
  child_style.align_self = Some(AlignItems::End);
  child_style.justify_self = Some(AlignItems::End);
  let child_style = Arc::new(child_style);

  let mut child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
  child.id = 2;
  let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![child]);
  grid.id = 1;

  let mut root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![grid]);
  root.id = 3;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let child_fragment = find_first_fragment_with_id(&fragment, 2).expect("child fragment");

  assert_approx(
    child_fragment.bounds.x(),
    80.0,
    "block-axis end alignment should affect x",
  );
  assert_approx(
    child_fragment.bounds.y(),
    40.0,
    "inline-axis end alignment should affect y",
  );
}
