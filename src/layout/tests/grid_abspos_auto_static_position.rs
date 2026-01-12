use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::AlignContent;
use fastrender::style::types::Direction;
use fastrender::style::types::GridTrack;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment<'a>(
  fragment: &'a fastrender::tree::fragment_tree::FragmentNode,
  id: usize,
) -> &'a fastrender::tree::fragment_tree::FragmentNode {
  fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(id))
    .unwrap_or_else(|| panic!("fragment with id {id} not found"))
}

#[test]
fn grid_abspos_auto_static_position_respects_padding() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(70.0)),
  ];
  let container_style = Arc::new(container_style);

  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.width = Some(Length::px(1.0));
  flow_style.height = Some(Length::px(1.0));
  flow_style.grid_column_start = 2;
  flow_style.grid_column_end = 3;
  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 1;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 1);
  assert_approx(
    abs_fragment.bounds.x(),
    10.0,
    "static position should start at the first track inside padding (x)",
  );
  assert_approx(
    abs_fragment.bounds.y(),
    5.0,
    "static position should start at the first track inside padding (y)",
  );
}

#[test]
fn grid_abspos_auto_span_static_position_respects_padding() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(70.0)),
  ];
  let container_style = Arc::new(container_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("auto / span 2".to_string());
  abs_style.grid_row_raw = Some("auto / span 2".to_string());
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 2;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 2);
  assert_approx(
    abs_fragment.bounds.x(),
    10.0,
    "auto/span placement should still align to the first track start (x)",
  );
  assert_approx(
    abs_fragment.bounds.y(),
    5.0,
    "auto/span placement should still align to the first track start (y)",
  );
}

#[test]
fn grid_abspos_span_auto_static_position_respects_padding() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));
  container_style.padding_left = Length::px(10.0);
  container_style.padding_top = Length::px(5.0);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(70.0)),
  ];
  let container_style = Arc::new(container_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("span 2 / auto".to_string());
  abs_style.grid_row_raw = Some("span 2 / auto".to_string());
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 3;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 3);
  assert_approx(
    abs_fragment.bounds.x(),
    10.0,
    "span/auto placement should still align to the first track start (x)",
  );
  assert_approx(
    abs_fragment.bounds.y(),
    5.0,
    "span/auto placement should still align to the first track start (y)",
  );
}

#[test]
fn grid_abspos_auto_static_position_respects_justify_content_center() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  container_style.justify_content = JustifyContent::Center;
  let container_style = Arc::new(container_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 4;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 20.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 4);
  assert_approx(
    abs_fragment.bounds.x(),
    50.0,
    "centered tracks should shift the static position (x)",
  );
}

#[test]
fn grid_abspos_auto_static_position_respects_align_content_center() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(20.0));
  container_style.height = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(20.0))];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(70.0)),
  ];
  container_style.align_content = AlignContent::Center;
  let container_style = Arc::new(container_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 5;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(20.0, 200.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 5);
  assert_approx(
    abs_fragment.bounds.y(),
    50.0,
    "centered row tracks should shift the static position (y)",
  );
}

#[test]
fn grid_abspos_auto_static_position_respects_padding_in_rtl() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.direction = Direction::Rtl;
  container_style.width = Some(Length::px(110.0));
  container_style.height = Some(Length::px(20.0));
  container_style.padding_left = Length::px(10.0);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  let container_style = Arc::new(container_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 6;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(110.0, 20.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 6);
  assert_approx(
    abs_fragment.bounds.x(),
    80.0,
    "rtl static position should track the first column start inside padding",
  );
}
