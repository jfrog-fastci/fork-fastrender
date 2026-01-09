use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_item_height_calc_rem_sizes_track() {
  // Regression test for converting calc() lengths with rem units into Taffy sizes.
  //
  // The MDN `text-orientation` fixture uses:
  //   height: calc(5.625rem + 1px)
  //
  // If we treat the rem term as a raw number during grid style conversion, the computed row height
  // becomes ~7px and subsequent rows overlap.
  let calc_height = {
    let rem = CalcLength::single(LengthUnit::Rem, 5.625);
    let px = CalcLength::single(LengthUnit::Px, 1.0);
    Length::calc(rem.add_scaled(&px, 1.0).expect("combine calc terms"))
  };
  let expected_height = 5.625 * 16.0 + 1.0;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(100.0));
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  container_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(100.0));
  first_style.height = Some(calc_height);
  let first_child =
    BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(100.0));
  second_style.height = Some(Length::px(10.0));
  let second_child =
    BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![first_child, second_child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 200.0))
    .expect("grid layout");

  let first_fragment = fragment
    .iter_fragments()
    .find(|node| node.style.as_ref().is_some_and(|style| style.height == Some(calc_height)))
    .expect("first grid item fragment");
  let second_fragment = fragment
    .iter_fragments()
    .find(|node| node.style.as_ref().is_some_and(|style| style.height == Some(Length::px(10.0))))
    .expect("second grid item fragment");

  assert_approx(
    first_fragment.bounds.height(),
    expected_height,
    "calc(rem + px) height should resolve to px during layout",
  );
  assert_approx(
    second_fragment.bounds.y(),
    expected_height,
    "second row should start after first row's resolved height",
  );
}

