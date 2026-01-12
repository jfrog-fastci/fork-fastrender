use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::types::WordBreak;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

#[test]
fn grid_container_width_max_content_shrinkwraps_tracks_and_gaps() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  container_style.grid_column_gap = Length::px(10.0);
  let container_style = Arc::new(container_style);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  let child_style = Arc::new(child_style);

  let mut grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![
      BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]),
      BoxNode::new_block(child_style, FormattingContextType::Block, vec![]),
    ],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(500.0, 200.0))
    .expect("layout succeeds");

  let expected = 40.0 + 60.0 + 10.0;
  let actual = fragment.bounds.width();
  assert!(
    (actual - expected).abs() < 0.5,
    "expected shrinkwrapped max-content width of {expected}, got {actual}",
  );
}

fn make_break_all_text_item(
  id: usize,
  width_keyword: Option<IntrinsicSizeKeyword>,
  text: &str,
  column: i32,
) -> BoxNode {
  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.font_size = 16.0;
  wrapper_style.word_break = WordBreak::BreakAll;
  wrapper_style.width_keyword = width_keyword;
  wrapper_style.grid_column_start = column;
  wrapper_style.grid_column_end = column + 1;
  let wrapper_style = Arc::new(wrapper_style);

  let mut text_style = ComputedStyle::default();
  text_style.font_size = 16.0;
  text_style.word_break = WordBreak::BreakAll;
  let text_style = Arc::new(text_style);

  let text_child = BoxNode::new_text(text_style, text.to_string());
  let mut item = BoxNode::new_block(
    wrapper_style,
    FormattingContextType::Inline,
    vec![text_child],
  );
  item.id = id;
  item
}

#[test]
fn grid_item_width_max_content_influences_track_sizing() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  let long_text = "a".repeat(64);
  let item_auto = make_break_all_text_item(11, None, &long_text, 1);
  let item_keyword =
    make_break_all_text_item(21, Some(IntrinsicSizeKeyword::MaxContent), &long_text, 1);

  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.width = Some(Length::px(10.0));
  sibling_style.height = Some(Length::px(10.0));
  sibling_style.grid_column_start = 2;
  sibling_style.grid_column_end = 3;
  let sibling_style = Arc::new(sibling_style);
  let mut sibling = BoxNode::new_block(sibling_style, FormattingContextType::Block, vec![]);
  sibling.id = 12;
  let mut sibling_keyword = sibling.clone();
  sibling_keyword.id = 22;

  let mut grid_auto = BoxNode::new_block(
    container_style.clone(),
    FormattingContextType::Grid,
    vec![item_auto, sibling],
  );
  grid_auto.id = 10;

  let mut grid_keyword = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item_keyword, sibling_keyword],
  );
  grid_keyword.id = 20;

  let fragment_auto = fc
    .layout(&grid_auto, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout auto");
  let fragment_keyword = fc
    .layout(&grid_keyword, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout keyword");

  let x_auto = fragment_auto.children[1].bounds.x();
  let x_keyword = fragment_keyword.children[1].bounds.x();
  assert!(
    x_keyword > x_auto + 10.0,
    "expected max-content width item to expand the first track (auto second column x={x_auto:.2}, keyword second column x={x_keyword:.2})",
  );
}
