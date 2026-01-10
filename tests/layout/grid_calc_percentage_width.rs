use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::GridTrack;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_item_width_calc_percentage_resolves_against_grid_area_width() {
  let fc = GridFormattingContext::new();

  let calc = |percent: f32, px: f32| -> Length {
    let calc = CalcLength::single(LengthUnit::Percent, percent)
      .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
      .expect("calc expression should be representable");
    Length::calc(calc)
  };

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(300.0));
  container_style.height = Some(Length::px(50.0));
  container_style.justify_content = JustifyContent::Start;
  container_style.align_content = AlignContent::Start;
  container_style.justify_items = AlignItems::Start;
  container_style.align_items = AlignItems::Start;
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(calc(100.0, -20.0));
  child_style.height = Some(Length::px(10.0));
  child_style.justify_self = Some(AlignItems::Start);
  child_style.align_self = Some(AlignItems::Start);

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );
  grid.id = 1;

  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  let child = fragment.children.first().expect("child fragment");
  assert_approx(child.bounds.width(), 280.0, "grid item width");
}

