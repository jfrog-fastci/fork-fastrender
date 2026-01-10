use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
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

  let mut grid =
    BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![child]);
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

