use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use fastrender::LayoutConstraints;
use std::sync::Arc;

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn inter_item_gap_px(fragment: &fastrender::tree::fragment_tree::FragmentNode) -> f32 {
  assert!(
    fragment.children.len() >= 2,
    "expected >= 2 grid item fragments, got {}",
    fragment.children.len()
  );
  let first = &fragment.children[0];
  let second = &fragment.children[1];
  second.bounds.x() - (first.bounds.x() + first.bounds.width())
}

#[test]
fn grid_calc_gap_is_resolved_per_layout_after_template_cache() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  grid_style.grid_column_gap_is_normal = false;
  grid_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0); // calc(10% - 5px)

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(10.0));
  item_style.height = Some(Length::px(10.0));

  let item_a = BoxNode::new_block(
    Arc::new(item_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let item_b = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item_a, item_b],
  );

  // Reuse the same formatting context so the style→Taffy template cache is exercised.
  let fc = GridFormattingContext::new();

  let fragment_200 = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 100.0))
    .expect("layout succeeds");
  assert_approx(
    inter_item_gap_px(&fragment_200),
    15.0,
    "gap should resolve against 200px base",
  );

  let fragment_400 = fc
    .layout(&grid, &LayoutConstraints::definite(400.0, 100.0))
    .expect("layout succeeds");
  assert_approx(
    inter_item_gap_px(&fragment_400),
    35.0,
    "gap should resolve against 400px base",
  );
}
