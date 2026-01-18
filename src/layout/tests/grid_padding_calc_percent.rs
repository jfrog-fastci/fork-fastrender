use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::GridTrack;
use crate::style::types::WritingMode;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::ComputedStyle;
use std::sync::Arc;

fn find_child_by_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  fragment.children.iter().find(|child| {
    matches!(
      child.content,
      FragmentContent::Block { box_id: Some(box_id) }
        | FragmentContent::Inline { box_id: Some(box_id), .. }
        | FragmentContent::Text { box_id: Some(box_id), .. }
        | FragmentContent::Replaced { box_id: Some(box_id), .. }
        if box_id == id
    )
  })
}

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn grid_container_padding_calc_with_percentage_resolves_against_containing_block_width() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(50.0));
  grid_style.width_keyword = None;
  grid_style.height_keyword = None;
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(10.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  grid_style.padding_left = calc_percent_plus_px(10.0, 5.0); // calc(10% + 5px) => 25px @ 200px base

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height_keyword = None;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 100;

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");

  let child_frag = find_child_by_id(&fragment, 1).expect("child fragment");
  assert!(
    (child_frag.bounds.x() - 25.0).abs() < 0.5,
    "expected child x≈25, got x={}",
    child_frag.bounds.x()
  );
}

#[test]
fn grid_container_padding_calc_with_percentage_resolves_against_physical_width_in_vertical_writing_mode(
) {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.writing_mode = WritingMode::VerticalRl;
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(50.0));
  grid_style.width_keyword = None;
  grid_style.height_keyword = None;
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(10.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  grid_style.padding_left = calc_percent_plus_px(10.0, 5.0); // calc(10% + 5px) => 25px @ 200px base

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height_keyword = None;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 100;

  // In vertical writing modes the containing block's physical width is the **block** axis.
  let constraints =
    LayoutConstraints::definite(80.0, 200.0).with_block_percentage_base(Some(200.0));
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&grid, &constraints).expect("layout succeeds");

  let child_frag = find_child_by_id(&fragment, 1).expect("child fragment");
  assert!(
    (child_frag.bounds.x() - 25.0).abs() < 0.5,
    "expected child x≈25, got x={}",
    child_frag.bounds.x()
  );
}
