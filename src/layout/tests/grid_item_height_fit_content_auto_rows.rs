use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexDirection;
use crate::style::types::GridTrack;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::WordBreak;
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextFactory;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn make_wrapping_text_box(text: &str) -> BoxNode {
  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.font_size = 16.0;
  wrapper_style.word_break = WordBreak::BreakAll;
  let wrapper_style = Arc::new(wrapper_style);

  let mut text_style = ComputedStyle::default();
  text_style.font_size = 16.0;
  text_style.word_break = WordBreak::BreakAll;
  let text = BoxNode::new_text(Arc::new(text_style), text.to_string());

  BoxNode::new_block(wrapper_style, FormattingContextType::Inline, vec![text])
}

#[test]
fn grid_item_height_fit_content_does_not_clamp_to_auto_row_probe() {
  // Regression test for `height: fit-content` on grid items.
  //
  // Some real-world layouts (si.edu) set `height: fit-content` on direct grid children while the
  // grid uses implicit `auto` rows. The grid area's block size is not definite in that case, so
  // `fit-content` should behave like `max-content` and size to the content's height, rather than
  // clamping to intermediate probe sizes passed during track sizing.

  let long_text = "a".repeat(256);

  let mut flex_base_style = ComputedStyle::default();
  flex_base_style.display = Display::Flex;
  flex_base_style.flex_direction = FlexDirection::Column;
  flex_base_style.grid_row_gap = Length::px(24.0);

  let flex_auto = BoxNode::new_block(
    Arc::new(flex_base_style.clone()),
    FormattingContextType::Flex,
    vec![
      make_wrapping_text_box(&long_text),
      make_wrapping_text_box(&long_text),
    ],
  );

  let factory = FormattingContextFactory::new();
  let flex_fc = factory.create(FormattingContextType::Flex);
  let expected_fragment = flex_fc
    .layout(&flex_auto, &LayoutConstraints::definite_width(200.0))
    .expect("flex layout for expected size");
  let expected_height = expected_fragment.bounds.height();
  assert!(
    expected_height > 0.0,
    "expected non-zero content height for regression"
  );

  let mut flex_fit_style = flex_base_style;
  flex_fit_style.height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
  flex_fit_style.grid_row_start = 1;
  flex_fit_style.grid_row_end = 2;
  flex_fit_style.grid_column_start = 1;
  flex_fit_style.grid_column_end = 2;
  let flex_fit = BoxNode::new_block(
    Arc::new(flex_fit_style),
    FormattingContextType::Flex,
    vec![
      make_wrapping_text_box(&long_text),
      make_wrapping_text_box(&long_text),
    ],
  );

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.height = Some(Length::px(10.0));
  second_style.grid_row_start = 2;
  second_style.grid_row_end = 3;
  second_style.grid_column_start = 1;
  second_style.grid_column_end = 2;
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![flex_fit, second],
  );

  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(
      &grid,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(500.0),
      ),
    )
    .expect("grid layout");

  assert_eq!(
    fragment.children.len(),
    2,
    "expected two grid item fragments"
  );
  let fit_fragment = &fragment.children[0];
  let second_fragment = &fragment.children[1];

  assert_approx(
    fit_fragment.bounds.height(),
    expected_height,
    "auto-row grid item with `height:fit-content` should size to content height",
  );
  assert_approx(
    second_fragment.bounds.y(),
    expected_height,
    "second row should start after the fit-content row",
  );
}
