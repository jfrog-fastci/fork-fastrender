use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::style::display::Display;
use crate::style::position::Position;
use crate::style::types::AlignContent;
use crate::style::types::AlignItems;
use crate::style::types::AspectRatio;
use crate::style::types::Direction;
use crate::style::types::GridTrack;
use crate::style::types::JustifyContent;
use crate::style::types::WritingMode;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use std::collections::HashMap;
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

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

fn synthesize_area_line_names(style: &mut ComputedStyle) {
  if style.grid_template_areas.is_empty() {
    return;
  }
  if let Some(bounds) = crate::style::grid::validate_area_rectangles(&style.grid_template_areas) {
    let ensure_line = |lines: &mut Vec<Vec<String>>,
                       names: &mut HashMap<String, Vec<usize>>,
                       idx: usize,
                       name: String| {
      if lines.len() <= idx {
        lines.resize(idx + 1, Vec::new());
      }
      if !lines[idx].contains(&name) {
        lines[idx].push(name.clone());
        names.entry(name).or_default().push(idx);
      }
    };

    if style.grid_column_line_names.len() < style.grid_template_columns.len() + 1 {
      style
        .grid_column_line_names
        .resize(style.grid_template_columns.len() + 1, Vec::new());
    }
    if style.grid_row_line_names.len() < style.grid_template_rows.len() + 1 {
      style
        .grid_row_line_names
        .resize(style.grid_template_rows.len() + 1, Vec::new());
    }

    for (name, (top, bottom, left, right)) in bounds {
      let col_start = left;
      let col_end = right + 1;
      let row_start = top;
      let row_end = bottom + 1;

      ensure_line(
        &mut style.grid_column_line_names,
        &mut style.grid_column_names,
        col_start,
        format!("{name}-start"),
      );
      ensure_line(
        &mut style.grid_column_line_names,
        &mut style.grid_column_names,
        col_end,
        format!("{name}-end"),
      );
      ensure_line(
        &mut style.grid_row_line_names,
        &mut style.grid_row_names,
        row_start,
        format!("{name}-start"),
      );
      ensure_line(
        &mut style.grid_row_line_names,
        &mut style.grid_row_names,
        row_end,
        format!("{name}-end"),
      );
    }
  }
}

#[test]
fn subgrid_area_line_name_inheritance_clamps_partial_overlap_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_template_areas = vec![vec![
    Some("main".into()),
    Some("main".into()),
    Some("main".into()),
    Some("main".into()),
    Some("main".into()),
  ]];
  parent_style.width = Some(Length::px(150.0));
  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  // Parent columns: [10,20,30,40,50]. Span columns 2-4 (start/end lie *inside* the `main` area).
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 5;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 2;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
  child_style.grid_column_raw = Some("main-start / main-end".into());
  child_style.grid_row_start = 1;
  child_style.grid_row_end = 2;
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let child_fragment = &subgrid_fragment.children[0];

  assert_approx(
    subgrid_fragment.bounds.x(),
    10.0,
    "subgrid begins after the first parent column",
  );
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "main-start clamps to the subgrid start line",
  );
  assert_approx(
    child_fragment.bounds.width(),
    90.0,
    "main-end clamps to the subgrid end line",
  );
}

#[test]
fn subgrid_area_line_name_inheritance_clamps_partial_overlap_rows() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_areas = vec![
    vec![Some("main".into())],
    vec![Some("main".into())],
    vec![Some("main".into())],
    vec![None],
  ];
  parent_style.width = Some(Length::px(80.0));
  parent_style.height = Some(Length::px(100.0));
  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  // Parent rows: [10,20,30,40]. Span rows 2-4 (start lies inside `main`).
  subgrid_style.grid_row_start = 2;
  subgrid_style.grid_row_end = 5;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 2;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.grid_row_raw = Some("main-start / main-end".into());
  child_style.grid_column_start = 1;
  child_style.grid_column_end = 2;
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let child_fragment = &subgrid_fragment.children[0];

  assert_approx(
    subgrid_fragment.bounds.y(),
    10.0,
    "subgrid begins after the first parent row",
  );
  assert_approx(
    child_fragment.bounds.y(),
    0.0,
    "main-start clamps to the subgrid start line",
  );
  assert_approx(
    child_fragment.bounds.height(),
    50.0,
    "main-end uses the parent's area end line within the subgrid",
  );
}

#[test]
fn subgrid_area_line_names_resolve_for_absolute_static_position() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(10.0))];
  parent_style.grid_template_areas = vec![vec![
    None,
    None,
    Some("mid".into()),
    Some("mid".into()),
    Some("mid".into()),
  ]];
  parent_style.width = Some(Length::px(150.0));
  parent_style.height = Some(Length::px(10.0));
  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  // Span parent columns 2-4 so `mid-end` lies outside the subgrid and must clamp to its end line.
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 5;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 2;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(5.0));
  abs_style.height = Some(Length::px(5.0));
  abs_style.grid_column_raw = Some("mid-start / mid-end".into());
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;
  let abs = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(
    subgrid_fragment.children.len(),
    1,
    "subgrid has exactly one absolutely-positioned child fragment"
  );
  let abs_fragment = &subgrid_fragment.children[0];
  assert_approx(
    abs_fragment.bounds.x(),
    20.0,
    "absolute child resolves `mid-start / mid-end` to the subgrid-clamped area",
  );
}

#[test]
fn nested_subgrids_clamp_area_line_names_at_each_level() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(10.0))];
  parent_style.grid_template_areas = vec![vec![
    Some("gutter".into()),
    Some("info".into()),
    Some("info".into()),
    Some("photos".into()),
  ]];
  parent_style.width = Some(Length::px(100.0));
  parent_style.height = Some(Length::px(10.0));
  synthesize_area_line_names(&mut parent_style);

  let mut middle_style = ComputedStyle::default();
  middle_style.display = Display::Grid;
  middle_style.grid_column_subgrid = true;
  middle_style.grid_row_subgrid = true;
  // Span parent columns 2-4.
  middle_style.grid_column_start = 2;
  middle_style.grid_column_end = 5;
  middle_style.grid_row_start = 1;
  middle_style.grid_row_end = 2;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_column_subgrid = true;
  inner_style.grid_row_subgrid = true;
  // Within `middle`, span columns 2-3 (which correspond to the parent's columns 3-4).
  inner_style.grid_column_start = 2;
  inner_style.grid_column_end = 4;
  inner_style.grid_row_start = 1;
  inner_style.grid_row_end = 2;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
  child_style.grid_column_raw = Some("info-start / info-end".into());
  child_style.grid_row_start = 1;
  child_style.grid_row_end = 2;

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![child],
  );
  let middle = BoxNode::new_block(
    Arc::new(middle_style),
    FormattingContextType::Grid,
    vec![inner],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![middle],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let middle_fragment = &fragment.children[0];
  let inner_fragment = &middle_fragment.children[0];
  let child_fragment = &inner_fragment.children[0];

  assert_approx(
    middle_fragment.bounds.x(),
    10.0,
    "middle starts after the `gutter` column",
  );
  assert_approx(
    inner_fragment.bounds.x(),
    20.0,
    "inner starts after the first inherited track in `middle`",
  );
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "info-start is clamped to the inner subgrid's start line",
  );
  assert_approx(
    child_fragment.bounds.width(),
    30.0,
    "info-start/info-end spans only the overlapping portion of the original area",
  );
}

#[test]
fn subgrid_contributes_to_parent_row_track_sizing() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));
  child1_style.grid_row_start = 1;
  child1_style.grid_row_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(50.0));
  child2_style.grid_row_start = 2;
  child2_style.grid_row_end = 3;

  let inner1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let inner2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![inner1, inner2],
  );

  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.height = Some(Length::px(5.0));
  sibling_style.grid_row_start = 2;
  sibling_style.grid_row_end = 3;
  let sibling = BoxNode::new_block(
    Arc::new(sibling_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid, sibling],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let sibling_fragment = &fragment.children[1];
  let inner_second = &subgrid_fragment.children[1];

  assert_approx(sibling_fragment.bounds.y(), 10.0, "second row start");
  assert_approx(
    sibling_fragment.bounds.y(),
    inner_second.bounds.y(),
    "parent row line matches subgrid row",
  );
}

#[test]
fn subgrid_contributes_to_parent_column_track_sizing() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(200.0));
  // Prevent `justify-content: normal` (the default) from stretching auto tracks, which would make
  // the expected track offsets dependent on the remaining free space rather than the intrinsic
  // contributions we want to assert here.
  parent_style.justify_content = JustifyContent::Start;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut col1_style = ComputedStyle::default();
  col1_style.display = Display::Block;
  col1_style.width = Some(Length::px(20.0));
  col1_style.grid_column_start = 1;
  col1_style.grid_column_end = 2;

  let mut col2_style = ComputedStyle::default();
  col2_style.display = Display::Block;
  col2_style.width = Some(Length::px(60.0));
  col2_style.grid_column_start = 2;
  col2_style.grid_column_end = 3;

  let inner1 = BoxNode::new_block(Arc::new(col1_style), FormattingContextType::Block, vec![]);
  let inner2 = BoxNode::new_block(Arc::new(col2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![inner1, inner2],
  );

  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.width = Some(Length::px(5.0));
  sibling_style.grid_column_start = 2;
  sibling_style.grid_column_end = 3;
  let sibling = BoxNode::new_block(
    Arc::new(sibling_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid, sibling],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let sibling_fragment = &fragment.children[1];
  let inner_second = &subgrid_fragment.children[1];

  assert_approx(sibling_fragment.bounds.x(), 20.0, "second column start");
  assert_approx(
    sibling_fragment.bounds.x(),
    inner_second.bounds.x(),
    "parent column line matches subgrid column",
  );
}

#[test]
fn row_subgrid_uses_parent_tracks() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));
  child1_style.grid_row_start = 1;
  child1_style.grid_row_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(50.0));
  child2_style.grid_row_start = 2;
  child2_style.grid_row_end = 3;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.y(), 0.0, "first row starts at 0");
  assert_approx(first.bounds.height(), 10.0, "first row height");
  assert_approx(
    second.bounds.y(),
    10.0,
    "second row offset matches first height",
  );
  assert_approx(second.bounds.height(), 50.0, "second row height");
}

#[test]
fn column_subgrid_aligns_with_parent_tracks() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(100.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut col_child1 = ComputedStyle::default();
  col_child1.display = Display::Block;
  col_child1.height = Some(Length::px(10.0));

  let mut col_child2 = ComputedStyle::default();
  col_child2.display = Display::Block;
  col_child2.height = Some(Length::px(10.0));

  let sub_child1 = BoxNode::new_block(Arc::new(col_child1), FormattingContextType::Block, vec![]);
  let sub_child2 = BoxNode::new_block(Arc::new(col_child2), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![sub_child1, sub_child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.x(), 0.0, "first column origin");
  assert_approx(first.bounds.width(), 40.0, "first column width");
  assert_approx(second.bounds.x(), 40.0, "second column offset");
  assert_approx(second.bounds.width(), 60.0, "second column width");
}

#[test]
fn layout_containment_disables_column_subgrid_track_inheritance() {
  // Per CSS Grid 2 §9.7 ("Subgrid"), layout containment forces an independent formatting context,
  // which disables subgrid and makes the used value `grid-template-columns/none`.
  //
  // The child grid requests `grid-template-columns: subgrid`, but has `contain: layout`, so it must
  // fall back to its own implicit tracks (from `grid-auto-columns`) rather than inheriting the
  // parent grid's fixed track sizes.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Grid;
  child_style.grid_column_subgrid = true;
  child_style.containment.layout = true;
  child_style.grid_column_start = 1;
  child_style.grid_column_end = 3;
  child_style.grid_row_start = 1;
  child_style.grid_row_end = 2;
  child_style.grid_auto_columns = vec![GridTrack::Length(Length::px(15.0))].into();

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(5.0));
  item_style.height = Some(Length::px(5.0));
  item_style.grid_column_start = 2;
  item_style.grid_column_end = 3;
  item_style.grid_row_start = 1;
  item_style.grid_row_end = 2;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Grid,
    vec![item],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let child_fragment = &fragment.children[0];
  let item_fragment = &child_fragment.children[0];
  assert_approx(
    item_fragment.bounds.x(),
    15.0,
    "contained subgrid uses grid-auto-columns, not the parent track size",
  );
}

#[test]
fn layout_containment_disables_subgrid_but_grid_uses_local_writing_mode() {
  // Per CSS Grid 2 §9.7 ("Subgrid"), layout containment forces an independent formatting context,
  // which disables subgrid and makes the used value `grid-template-*: none`.
  //
  // Ensure a contained "would-be subgrid" behaves like an independent grid, using its own
  // `writing-mode` for axis mapping (columns -> physical Y in vertical writing modes).
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Length(Length::px(60.0))];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(60.0))];
  parent_style.width = Some(Length::px(60.0));
  parent_style.height = Some(Length::px(60.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Grid;
  child_style.grid_column_subgrid = true;
  child_style.containment.layout = true;
  child_style.writing_mode = WritingMode::VerticalLr;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(30.0));
  child_style.grid_column_start = 1;
  child_style.grid_column_end = 2;
  child_style.grid_row_start = 1;
  child_style.grid_row_end = 2;
  child_style.grid_auto_columns = vec![GridTrack::Length(Length::px(15.0))].into();
  child_style.grid_auto_rows = vec![GridTrack::Length(Length::px(10.0))].into();

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(5.0));
  item_style.height = Some(Length::px(5.0));
  item_style.grid_column_start = 2;
  item_style.grid_column_end = 3;
  item_style.grid_row_start = 1;
  item_style.grid_row_end = 2;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Grid,
    vec![item],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let child_fragment = &fragment.children[0];
  let item_fragment = &child_fragment.children[0];
  assert_approx(
    item_fragment.bounds.x(),
    0.0,
    "vertical writing-mode maps grid rows to the physical x-axis",
  );
  assert_approx(
    item_fragment.bounds.y(),
    15.0,
    "vertical writing-mode maps grid columns to the physical y-axis",
  );
}

#[test]
fn subgrid_autoplacement_uses_parent_rows_and_gaps() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_row_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(15.0));

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(25.0));

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.y(), 0.0, "first row origin");
  assert_approx(first.bounds.height(), 15.0, "first row size");
  assert_approx(second.bounds.y(), 20.0, "second row offset includes gap");
  assert_approx(second.bounds.height(), 25.0, "second row size");
}

#[test]
fn column_subgrid_inherits_gaps_for_autoplacement() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(10.0));

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.x(), 0.0, "first column origin");
  assert_approx(first.bounds.width(), 30.0, "first column width");
  assert_approx(second.bounds.x(), 35.0, "second column offset includes gap");
  assert_approx(second.bounds.width(), 40.0, "second column width");
}

#[test]
fn subgrid_autospan_prefers_line_name_list_length() {
  // Matches WPT `subgrid-auto-span-001`: when the subgrid axis has an explicit `<line-name-list>`
  // (more than the single empty placeholder), auto-placement should treat the subgrid item as
  // `auto / span (<line-name-list length - 1>)` even when the parent has more tracks.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(70.0)),
    GridTrack::Length(Length::px(90.0)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  parent_style.grid_column_gap = Length::px(10.0);
  parent_style.justify_content = JustifyContent::Start;
  parent_style.width = Some(Length::px(230.0));
  parent_style.height = Some(Length::px(40.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.height = Some(Length::px(20.0));
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.subgrid_column_line_names =
    vec![vec!["a".into()], vec!["b".into()], vec!["c".into()]];
  // `grid-template-columns: subgrid ...` stores the same line-name lists on `grid_column_line_names`.
  subgrid_style.grid_column_line_names = subgrid_style.subgrid_column_line_names.clone();
  let subgrid = BoxNode::new_block(Arc::new(subgrid_style), FormattingContextType::Grid, vec![]);

  let mut third_style = ComputedStyle::default();
  third_style.display = Display::Block;
  third_style.height = Some(Length::px(20.0));
  let third = BoxNode::new_block(Arc::new(third_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![first, subgrid, third],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(230.0, 60.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 3);
  let third_fragment = &fragment.children[2];
  assert_approx(
    third_fragment.bounds.y(),
    20.0,
    "line-name list length should force the subgrid item to span 2 columns, pushing the third item to row 2",
  );
}

#[test]
fn subgrid_autospan_uses_parent_track_count_when_no_line_names() {
  // Matches WPT `subgrid-nested-writing-mode-001`: when the subgrid axis is plain `subgrid` (no
  // line-name list provided), auto-placement should treat the subgrid item as spanning the parent's
  // explicit track count.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(42.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.justify_content = JustifyContent::Start;
  parent_style.width = Some(Length::px(75.0));

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.writing_mode = WritingMode::VerticalRl;
  outer_style.grid_column_subgrid = true;
  outer_style.grid_row_subgrid = true;
  // Avoid `justify/align-content: normal` distributing free space in ways that would make the
  // inherited track offsets dependent on the available size rather than the parent track list.
  outer_style.justify_content = JustifyContent::Start;
  outer_style.align_content = AlignContent::Start;
  outer_style.subgrid_column_line_names = vec![];
  outer_style.subgrid_row_line_names = vec![];
  outer_style.grid_column_line_names = outer_style.subgrid_column_line_names.clone();
  outer_style.grid_row_line_names = outer_style.subgrid_row_line_names.clone();

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.writing_mode = WritingMode::VerticalRl;
  inner_style.grid_column_subgrid = true;
  inner_style.grid_row_subgrid = true;
  inner_style.justify_content = JustifyContent::Start;
  inner_style.align_content = AlignContent::Start;
  inner_style.subgrid_column_line_names = vec![];
  inner_style.subgrid_row_line_names = vec![];
  inner_style.grid_column_line_names = inner_style.subgrid_column_line_names.clone();
  inner_style.grid_row_line_names = inner_style.subgrid_row_line_names.clone();

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.justify_self = Some(AlignItems::Start);
  a_style.align_self = Some(AlignItems::Start);
  a_style.grid_column_start = 1;
  a_style.grid_column_end = 2;
  a_style.grid_row_start = 1;
  a_style.grid_row_end = 2;
  a_style.height = Some(Length::px(12.0));

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  b_style.justify_self = Some(AlignItems::Start);
  b_style.align_self = Some(AlignItems::Start);
  b_style.grid_column_start = 2;
  b_style.grid_column_end = 3;
  b_style.grid_row_start = 1;
  b_style.grid_row_end = 2;
  b_style.height = Some(Length::px(12.0));

  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);
  let b = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);

  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![a, b],
  );
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(75.0, 50.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  assert_eq!(outer_fragment.children.len(), 1);
  let inner_fragment = &outer_fragment.children[0];
  assert_eq!(inner_fragment.children.len(), 2);

  assert_approx(
    outer_fragment.bounds.width(),
    75.0,
    "outer subgrid should span both parent columns when auto-placed",
  );

  let first = &inner_fragment.children[0];
  let second = &inner_fragment.children[1];
  assert_approx(
    first.bounds.x(),
    0.0,
    "first inherited column starts at origin on the physical X axis",
  );
  assert_approx(
    second.bounds.x(),
    33.0,
    "second inherited column offset includes the parent gap on the physical X axis",
  );
}

#[test]
fn subgrid_column_gap_can_differ_from_parent_gap() {
  fn run(column_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_rows = vec![GridTrack::Auto];
    parent_style.grid_column_gap = Length::px(50.0);
    parent_style.width = Some(Length::px(300.0));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.grid_column_start = 1;
    subgrid_style.grid_column_end = 3;
    if let Some((gap, is_normal)) = column_gap {
      subgrid_style.grid_column_gap = gap;
      subgrid_style.grid_column_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.height = Some(Length::px(10.0));
    child1_style.grid_column_start = 1;
    child1_style.grid_column_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.height = Some(Length::px(10.0));
    child2_style.grid_column_start = 2;
    child2_style.grid_column_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(300.0, 100.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let left = &subgrid_fragment.children[0];
    let right = &subgrid_fragment.children[1];
    (
      left.bounds.width(),
      right.bounds.x(),
      right.bounds.width(),
      right.bounds.x() + right.bounds.width(),
    )
  }

  // `column-gap: normal` (the default) matches the parent.
  let (left_width, right_x, right_width, right_end) = run(None);
  assert_approx(left_width, 100.0, "normal gap left width");
  assert_approx(right_x, 150.0, "normal gap right x");
  assert_approx(right_width, 150.0, "normal gap right width");
  assert_approx(right_end, 300.0, "normal gap right end");

  // `column-gap: 0` shrinks the visual gutter by applying half the difference (-25px) as margins.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(0.0), false)));
  assert_approx(left_width, 125.0, "0px gap left width");
  assert_approx(right_x, 125.0, "0px gap right x");
  assert_approx(right_width, 175.0, "0px gap right width");
  assert_approx(right_end, 300.0, "0px gap right end");

  // Intermediate values split the difference.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(25.0), false)));
  assert_approx(left_width, 112.5, "25px gap left width");
  assert_approx(right_x, 137.5, "25px gap right x");
  assert_approx(right_width, 162.5, "25px gap right width");
  assert_approx(right_end, 300.0, "25px gap right end");
}

#[test]
fn subgrid_row_gap_can_differ_from_parent_gap() {
  fn run(row_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_rows =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_columns = vec![GridTrack::Auto];
    parent_style.grid_row_gap = Length::px(50.0);
    parent_style.width = Some(Length::px(100.0));
    parent_style.height = Some(Length::px(300.0));
    parent_style.align_items = AlignItems::Stretch;

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_row_start = 1;
    subgrid_style.grid_row_end = 3;
    subgrid_style.align_items = AlignItems::Stretch;
    if let Some((gap, is_normal)) = row_gap {
      subgrid_style.grid_row_gap = gap;
      subgrid_style.grid_row_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.grid_row_start = 1;
    child1_style.grid_row_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.grid_row_start = 2;
    child2_style.grid_row_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(100.0, 300.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let top = &subgrid_fragment.children[0];
    let bottom = &subgrid_fragment.children[1];
    (
      top.bounds.height(),
      bottom.bounds.y(),
      bottom.bounds.height(),
      bottom.bounds.y() + bottom.bounds.height(),
    )
  }

  // `row-gap: normal` (the default) matches the parent.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(None);
  assert_approx(top_height, 100.0, "normal gap top height");
  assert_approx(bottom_y, 150.0, "normal gap bottom y");
  assert_approx(bottom_height, 150.0, "normal gap bottom height");
  assert_approx(bottom_end, 300.0, "normal gap bottom end");

  // `row-gap: 0` shrinks the visual gutter by applying half the difference (-25px) as margins.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(0.0), false)));
  assert_approx(top_height, 125.0, "0px gap top height");
  assert_approx(bottom_y, 125.0, "0px gap bottom y");
  assert_approx(bottom_height, 175.0, "0px gap bottom height");
  assert_approx(bottom_end, 300.0, "0px gap bottom end");

  // Intermediate values split the difference.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(25.0), false)));
  assert_approx(top_height, 112.5, "25px gap top height");
  assert_approx(bottom_y, 137.5, "25px gap bottom y");
  assert_approx(bottom_height, 162.5, "25px gap bottom height");
  assert_approx(bottom_end, 300.0, "25px gap bottom end");
}

#[test]
fn subgrid_column_gap_difference_resolves_percentage_gap() {
  fn run(column_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_rows = vec![GridTrack::Auto];
    parent_style.grid_column_gap_is_normal = false;
    parent_style.grid_column_gap = Length::percent(10.0);
    parent_style.width = Some(Length::px(300.0));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.grid_column_start = 1;
    subgrid_style.grid_column_end = 3;
    if let Some((gap, is_normal)) = column_gap {
      subgrid_style.grid_column_gap = gap;
      subgrid_style.grid_column_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.height = Some(Length::px(10.0));
    child1_style.grid_column_start = 1;
    child1_style.grid_column_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.height = Some(Length::px(10.0));
    child2_style.grid_column_start = 2;
    child2_style.grid_column_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(300.0, 100.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let left = &subgrid_fragment.children[0];
    let right = &subgrid_fragment.children[1];
    (
      left.bounds.width(),
      right.bounds.x(),
      right.bounds.width(),
      right.bounds.x() + right.bounds.width(),
    )
  }

  // Parent gap is 10% of 300px => 30px; `column-gap: normal` matches parent (delta = 0).
  let (left_width, right_x, right_width, right_end) = run(None);
  assert_approx(left_width, 100.0, "normal gap left width");
  assert_approx(right_x, 130.0, "normal gap right x");
  assert_approx(right_width, 170.0, "normal gap right width");
  assert_approx(right_end, 300.0, "normal gap right end");

  // Explicitly setting `column-gap: normal` should also inherit the parent's resolved gap.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(0.0), true)));
  assert_approx(left_width, 100.0, "explicit normal gap left width");
  assert_approx(right_x, 130.0, "explicit normal gap right x");
  assert_approx(right_width, 170.0, "explicit normal gap right width");
  assert_approx(right_end, 300.0, "explicit normal gap right end");

  // `column-gap: 0` shrinks the visual gutter by half-delta margins: (0px - 30px) / 2 = -15px.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(0.0), false)));
  assert_approx(left_width, 115.0, "0px gap left width");
  assert_approx(right_x, 115.0, "0px gap right x");
  assert_approx(right_width, 185.0, "0px gap right width");
  assert_approx(right_end, 300.0, "0px gap right end");

  // Intermediate values split the difference: (25px - 30px) / 2 = -2.5px.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(25.0), false)));
  assert_approx(left_width, 102.5, "25px gap left width");
  assert_approx(right_x, 127.5, "25px gap right x");
  assert_approx(right_width, 172.5, "25px gap right width");
  assert_approx(right_end, 300.0, "25px gap right end");
}

#[test]
fn subgrid_column_gap_difference_resolves_calc_gap() {
  fn run(column_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_rows = vec![GridTrack::Auto];
    parent_style.grid_column_gap_is_normal = false;
    parent_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0);
    parent_style.width = Some(Length::px(300.0));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.grid_column_start = 1;
    subgrid_style.grid_column_end = 3;
    if let Some((gap, is_normal)) = column_gap {
      subgrid_style.grid_column_gap = gap;
      subgrid_style.grid_column_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.height = Some(Length::px(10.0));
    child1_style.grid_column_start = 1;
    child1_style.grid_column_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.height = Some(Length::px(10.0));
    child2_style.grid_column_start = 2;
    child2_style.grid_column_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(300.0, 100.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let left = &subgrid_fragment.children[0];
    let right = &subgrid_fragment.children[1];
    (
      left.bounds.width(),
      right.bounds.x(),
      right.bounds.width(),
      right.bounds.x() + right.bounds.width(),
    )
  }

  // Parent gap is calc(10% - 5px) => calc(30px - 5px) = 25px.
  let (left_width, right_x, right_width, right_end) = run(None);
  assert_approx(left_width, 100.0, "normal gap left width");
  assert_approx(right_x, 125.0, "normal gap right x");
  assert_approx(right_width, 175.0, "normal gap right width");
  assert_approx(right_end, 300.0, "normal gap right end");

  // Explicitly setting `column-gap: normal` should also inherit the parent's resolved gap.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(0.0), true)));
  assert_approx(left_width, 100.0, "explicit normal gap left width");
  assert_approx(right_x, 125.0, "explicit normal gap right x");
  assert_approx(right_width, 175.0, "explicit normal gap right width");
  assert_approx(right_end, 300.0, "explicit normal gap right end");

  // `column-gap: 0` => (0px - 25px) / 2 = -12.5px.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(0.0), false)));
  assert_approx(left_width, 112.5, "0px gap left width");
  assert_approx(right_x, 112.5, "0px gap right x");
  assert_approx(right_width, 187.5, "0px gap right width");
  assert_approx(right_end, 300.0, "0px gap right end");

  // Explicitly setting the same gap as the parent should result in zero delta.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::px(25.0), false)));
  assert_approx(left_width, 100.0, "25px gap left width");
  assert_approx(right_x, 125.0, "25px gap right x");
  assert_approx(right_width, 175.0, "25px gap right width");
  assert_approx(right_end, 300.0, "25px gap right end");
}

#[test]
fn subgrid_column_gap_difference_resolves_percentage_subgrid_gap() {
  fn run(column_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_rows = vec![GridTrack::Auto];
    parent_style.grid_column_gap = Length::px(50.0);
    parent_style.width = Some(Length::px(200.0));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.grid_column_start = 1;
    subgrid_style.grid_column_end = 3;
    if let Some((gap, is_normal)) = column_gap {
      subgrid_style.grid_column_gap = gap;
      subgrid_style.grid_column_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.height = Some(Length::px(10.0));
    child1_style.grid_column_start = 1;
    child1_style.grid_column_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.height = Some(Length::px(10.0));
    child2_style.grid_column_start = 2;
    child2_style.grid_column_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 100.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let left = &subgrid_fragment.children[0];
    let right = &subgrid_fragment.children[1];
    (
      left.bounds.width(),
      right.bounds.x(),
      right.bounds.width(),
      right.bounds.x() + right.bounds.width(),
    )
  }

  // Base case: parent gap is 50px; `column-gap: normal` matches parent (delta = 0).
  let (left_width, right_x, right_width, right_end) = run(None);
  assert_approx(left_width, 100.0, "normal gap left width");
  assert_approx(right_x, 150.0, "normal gap right x");
  assert_approx(right_width, 50.0, "normal gap right width");
  assert_approx(right_end, 200.0, "normal gap right end");

  // Subgrid gap is 10% of 200px => 20px; delta = 20px - 50px = -30px => half-delta = -15px.
  let (left_width, right_x, right_width, right_end) = run(Some((Length::percent(10.0), false)));
  assert_approx(left_width, 115.0, "10% gap left width");
  assert_approx(right_x, 135.0, "10% gap right x");
  assert_approx(right_width, 65.0, "10% gap right width");
  assert_approx(right_end, 200.0, "10% gap right end");
}

#[test]
fn subgrid_column_gap_difference_resolves_calc_subgrid_gap() {
  fn run(column_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_rows = vec![GridTrack::Auto];
    parent_style.grid_column_gap = Length::px(50.0);
    parent_style.width = Some(Length::px(200.0));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.grid_column_start = 1;
    subgrid_style.grid_column_end = 3;
    if let Some((gap, is_normal)) = column_gap {
      subgrid_style.grid_column_gap = gap;
      subgrid_style.grid_column_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.height = Some(Length::px(10.0));
    child1_style.grid_column_start = 1;
    child1_style.grid_column_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.height = Some(Length::px(10.0));
    child2_style.grid_column_start = 2;
    child2_style.grid_column_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 100.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let left = &subgrid_fragment.children[0];
    let right = &subgrid_fragment.children[1];
    (
      left.bounds.width(),
      right.bounds.x(),
      right.bounds.width(),
      right.bounds.x() + right.bounds.width(),
    )
  }

  // Subgrid gap is calc(10% - 5px) => calc(20px - 5px) = 15px.
  let (left_width, right_x, right_width, right_end) =
    run(Some((calc_percent_plus_px(10.0, -5.0), false)));
  // Delta = 15px - 50px = -35px => half-delta = -17.5px.
  assert_approx(left_width, 117.5, "calc gap left width");
  assert_approx(right_x, 132.5, "calc gap right x");
  assert_approx(right_width, 67.5, "calc gap right width");
  assert_approx(right_end, 200.0, "calc gap right end");
}

#[test]
fn subgrid_row_gap_difference_resolves_percentage_gap() {
  fn run(row_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_rows =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_columns = vec![GridTrack::Auto];
    parent_style.grid_row_gap_is_normal = false;
    parent_style.grid_row_gap = Length::percent(10.0);
    parent_style.width = Some(Length::px(300.0));
    parent_style.height = Some(Length::px(300.0));
    parent_style.align_items = AlignItems::Stretch;

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_row_start = 1;
    subgrid_style.grid_row_end = 3;
    subgrid_style.align_items = AlignItems::Stretch;
    if let Some((gap, is_normal)) = row_gap {
      subgrid_style.grid_row_gap = gap;
      subgrid_style.grid_row_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.grid_row_start = 1;
    child1_style.grid_row_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.grid_row_start = 2;
    child2_style.grid_row_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(300.0, 300.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let top = &subgrid_fragment.children[0];
    let bottom = &subgrid_fragment.children[1];
    (
      top.bounds.height(),
      bottom.bounds.y(),
      bottom.bounds.height(),
      bottom.bounds.y() + bottom.bounds.height(),
    )
  }

  // Parent gap is 10% of 300px => 30px; `row-gap: normal` matches parent (delta = 0).
  let (top_height, bottom_y, bottom_height, bottom_end) = run(None);
  assert_approx(top_height, 100.0, "normal gap top height");
  assert_approx(bottom_y, 130.0, "normal gap bottom y");
  assert_approx(bottom_height, 170.0, "normal gap bottom height");
  assert_approx(bottom_end, 300.0, "normal gap bottom end");

  // Explicitly setting `row-gap: normal` should also inherit the parent's resolved gap.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(0.0), true)));
  assert_approx(top_height, 100.0, "explicit normal gap top height");
  assert_approx(bottom_y, 130.0, "explicit normal gap bottom y");
  assert_approx(bottom_height, 170.0, "explicit normal gap bottom height");
  assert_approx(bottom_end, 300.0, "explicit normal gap bottom end");

  // `row-gap: 0` => (0px - 30px) / 2 = -15px.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(0.0), false)));
  assert_approx(top_height, 115.0, "0px gap top height");
  assert_approx(bottom_y, 115.0, "0px gap bottom y");
  assert_approx(bottom_height, 185.0, "0px gap bottom height");
  assert_approx(bottom_end, 300.0, "0px gap bottom end");

  // Intermediate values split the difference: (25px - 30px) / 2 = -2.5px.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(25.0), false)));
  assert_approx(top_height, 102.5, "25px gap top height");
  assert_approx(bottom_y, 127.5, "25px gap bottom y");
  assert_approx(bottom_height, 172.5, "25px gap bottom height");
  assert_approx(bottom_end, 300.0, "25px gap bottom end");
}

#[test]
fn subgrid_row_gap_difference_resolves_calc_gap() {
  fn run(row_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_rows =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_columns = vec![GridTrack::Auto];
    parent_style.grid_row_gap_is_normal = false;
    parent_style.grid_row_gap = calc_percent_plus_px(10.0, -5.0);
    parent_style.width = Some(Length::px(300.0));
    parent_style.height = Some(Length::px(300.0));
    parent_style.align_items = AlignItems::Stretch;

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_row_start = 1;
    subgrid_style.grid_row_end = 3;
    subgrid_style.align_items = AlignItems::Stretch;
    if let Some((gap, is_normal)) = row_gap {
      subgrid_style.grid_row_gap = gap;
      subgrid_style.grid_row_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.grid_row_start = 1;
    child1_style.grid_row_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.grid_row_start = 2;
    child2_style.grid_row_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(300.0, 300.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let top = &subgrid_fragment.children[0];
    let bottom = &subgrid_fragment.children[1];
    (
      top.bounds.height(),
      bottom.bounds.y(),
      bottom.bounds.height(),
      bottom.bounds.y() + bottom.bounds.height(),
    )
  }

  // Parent gap is calc(10% - 5px) => calc(30px - 5px) = 25px.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(None);
  assert_approx(top_height, 100.0, "normal gap top height");
  assert_approx(bottom_y, 125.0, "normal gap bottom y");
  assert_approx(bottom_height, 175.0, "normal gap bottom height");
  assert_approx(bottom_end, 300.0, "normal gap bottom end");

  // Explicitly setting `row-gap: normal` should also inherit the parent's resolved gap.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(0.0), true)));
  assert_approx(top_height, 100.0, "explicit normal gap top height");
  assert_approx(bottom_y, 125.0, "explicit normal gap bottom y");
  assert_approx(bottom_height, 175.0, "explicit normal gap bottom height");
  assert_approx(bottom_end, 300.0, "explicit normal gap bottom end");

  // `row-gap: 0` => (0px - 25px) / 2 = -12.5px.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(0.0), false)));
  assert_approx(top_height, 112.5, "0px gap top height");
  assert_approx(bottom_y, 112.5, "0px gap bottom y");
  assert_approx(bottom_height, 187.5, "0px gap bottom height");
  assert_approx(bottom_end, 300.0, "0px gap bottom end");

  // Explicitly setting the same gap as the parent should result in zero delta.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::px(25.0), false)));
  assert_approx(top_height, 100.0, "25px gap top height");
  assert_approx(bottom_y, 125.0, "25px gap bottom y");
  assert_approx(bottom_height, 175.0, "25px gap bottom height");
  assert_approx(bottom_end, 300.0, "25px gap bottom end");
}

#[test]
fn subgrid_row_gap_difference_resolves_percentage_subgrid_gap() {
  fn run(row_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_rows =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_columns = vec![GridTrack::Auto];
    parent_style.grid_row_gap = Length::px(50.0);
    // Make the inline size differ from the block size so percentage resolution is observable.
    parent_style.width = Some(Length::px(200.0));
    parent_style.height = Some(Length::px(300.0));
    parent_style.align_items = AlignItems::Stretch;

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_row_start = 1;
    subgrid_style.grid_row_end = 3;
    subgrid_style.align_items = AlignItems::Stretch;
    if let Some((gap, is_normal)) = row_gap {
      subgrid_style.grid_row_gap = gap;
      subgrid_style.grid_row_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.grid_row_start = 1;
    child1_style.grid_row_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.grid_row_start = 2;
    child2_style.grid_row_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 300.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let top = &subgrid_fragment.children[0];
    let bottom = &subgrid_fragment.children[1];
    (
      top.bounds.height(),
      bottom.bounds.y(),
      bottom.bounds.height(),
      bottom.bounds.y() + bottom.bounds.height(),
    )
  }

  // Base case: parent gap is 50px; `row-gap: normal` matches parent (delta = 0).
  let (top_height, bottom_y, bottom_height, bottom_end) = run(None);
  assert_approx(top_height, 100.0, "normal gap top height");
  assert_approx(bottom_y, 150.0, "normal gap bottom y");
  assert_approx(bottom_height, 150.0, "normal gap bottom height");
  assert_approx(bottom_end, 300.0, "normal gap bottom end");

  // Subgrid gap is 10% of 200px => 20px; delta = 20px - 50px = -30px => half-delta = -15px.
  let (top_height, bottom_y, bottom_height, bottom_end) = run(Some((Length::percent(10.0), false)));
  assert_approx(top_height, 115.0, "10% gap top height");
  assert_approx(bottom_y, 135.0, "10% gap bottom y");
  assert_approx(bottom_height, 165.0, "10% gap bottom height");
  assert_approx(bottom_end, 300.0, "10% gap bottom end");
}

#[test]
fn subgrid_row_gap_difference_resolves_calc_subgrid_gap() {
  fn run(row_gap: Option<(Length, bool)>) -> (f32, f32, f32, f32) {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;
    parent_style.grid_template_rows =
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)];
    parent_style.grid_template_columns = vec![GridTrack::Auto];
    parent_style.grid_row_gap = Length::px(50.0);
    parent_style.width = Some(Length::px(200.0));
    parent_style.height = Some(Length::px(300.0));
    parent_style.align_items = AlignItems::Stretch;

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = Display::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_row_start = 1;
    subgrid_style.grid_row_end = 3;
    subgrid_style.align_items = AlignItems::Stretch;
    if let Some((gap, is_normal)) = row_gap {
      subgrid_style.grid_row_gap = gap;
      subgrid_style.grid_row_gap_is_normal = is_normal;
    }

    let mut child1_style = ComputedStyle::default();
    child1_style.display = Display::Block;
    child1_style.grid_row_start = 1;
    child1_style.grid_row_end = 2;

    let mut child2_style = ComputedStyle::default();
    child2_style.display = Display::Block;
    child2_style.grid_row_start = 2;
    child2_style.grid_row_end = 3;

    let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

    let subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![child1, child2],
    );

    let grid = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );

    let fc = GridFormattingContext::new();
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 300.0))
      .expect("layout succeeds");

    let subgrid_fragment = &fragment.children[0];
    assert_eq!(subgrid_fragment.children.len(), 2);
    let top = &subgrid_fragment.children[0];
    let bottom = &subgrid_fragment.children[1];
    (
      top.bounds.height(),
      bottom.bounds.y(),
      bottom.bounds.height(),
      bottom.bounds.y() + bottom.bounds.height(),
    )
  }

  // Subgrid gap is calc(10% - 5px) => calc(20px - 5px) = 15px.
  let (top_height, bottom_y, bottom_height, bottom_end) =
    run(Some((calc_percent_plus_px(10.0, -5.0), false)));
  // Delta = 15px - 50px = -35px => half-delta = -17.5px.
  assert_approx(top_height, 117.5, "calc gap top height");
  assert_approx(bottom_y, 132.5, "calc gap bottom y");
  assert_approx(bottom_height, 167.5, "calc gap bottom height");
  assert_approx(bottom_end, 300.0, "calc gap bottom end");
}

#[test]
fn nested_subgrid_gap_difference_accumulates() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(100.0)),
    GridTrack::Auto,
    GridTrack::Auto,
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(50.0);
  parent_style.width = Some(Length::px(500.0));
  parent_style.height = Some(Length::px(50.0));
  parent_style.align_items = AlignItems::Stretch;
  parent_style.justify_items = AlignItems::Stretch;
  // Avoid the `justify-content: normal` used value of `stretch` distributing extra space into the
  // auto tracks, which would obscure the virtual-item margin contribution we're testing.
  parent_style.justify_content = JustifyContent::Start;

  // Outer subgrid spans all columns and requests a smaller gap than the parent.
  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_column_subgrid = true;
  outer_style.grid_column_start = 1;
  outer_style.grid_column_end = 4;
  outer_style.grid_row_start = 1;
  outer_style.grid_row_end = 2;
  outer_style.grid_column_gap = Length::px(0.0);
  outer_style.grid_column_gap_is_normal = false;

  // Inner subgrid sits away from the outer subgrid's inline-start edge, so it receives the
  // half-difference margin adjustment. That adjustment must propagate to its descendant virtual
  // items when they contribute to the parent track sizing algorithm.
  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_column_subgrid = true;
  inner_style.grid_column_start = 2;
  inner_style.grid_column_end = 4;
  inner_style.grid_row_start = 1;
  inner_style.grid_row_end = 2;
  // Ensure the inner subgrid element itself doesn't dominate track sizing; we're asserting about
  // descendant virtual items.
  inner_style.width = Some(Length::px(0.0));
  inner_style.justify_self = Some(AlignItems::Start);

  let mut leaf_style = ComputedStyle::default();
  leaf_style.display = Display::Block;
  leaf_style.width = Some(Length::px(100.0));
  leaf_style.height = Some(Length::px(10.0));
  leaf_style.grid_column_start = 1;
  leaf_style.grid_column_end = 2;

  let leaf = BoxNode::new_block(Arc::new(leaf_style), FormattingContextType::Block, vec![]);
  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![leaf],
  );
  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );

  // Place a probe item in the second row/third column so we can observe the computed column track
  // offsets. If the leaf item's accumulated -25px margin is ignored, column 2 will be sized to
  // 100px and the probe will start at x=300. With the accumulated margin it starts at x=275.
  let mut probe_style = ComputedStyle::default();
  probe_style.display = Display::Block;
  probe_style.width = Some(Length::px(1.0));
  probe_style.height = Some(Length::px(1.0));
  probe_style.grid_column_start = 3;
  probe_style.grid_column_end = 4;
  probe_style.grid_row_start = 2;
  probe_style.grid_row_end = 3;
  let probe = BoxNode::new_block(Arc::new(probe_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid, probe],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(500.0, 50.0))
    .expect("layout succeeds");

  let probe_fragment = &fragment.children[1];
  assert_approx(
    probe_fragment.bounds.x(),
    275.0,
    "nested subgrid margins accumulate into virtual item contributions",
  );
}

#[test]
fn column_subgrid_with_mismatched_writing_mode_transposes_tracks() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(24.0)),
    GridTrack::Length(Length::px(36.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(7.0);
  parent_style.width = Some(Length::px(120.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(6.0));
  child1_style.justify_self = Some(AlignItems::Start);
  child1_style.align_self = Some(AlignItems::Start);
  child1_style.grid_column_start = 1;
  child1_style.grid_column_end = 2;
  child1_style.grid_row_start = 1;
  child1_style.grid_row_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(6.0));
  child2_style.justify_self = Some(AlignItems::Start);
  child2_style.align_self = Some(AlignItems::Start);
  child2_style.grid_column_start = 2;
  child2_style.grid_column_end = 3;
  child2_style.grid_row_start = 1;
  child2_style.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(
    first.bounds.x(),
    0.0,
    "first row origin maps to the x-axis after transpose",
  );
  assert_approx(first.bounds.width(), 6.0, "row height becomes item width");
  assert_approx(first.bounds.y(), 0.0, "first column starts at origin");
  assert_approx(first.bounds.height(), 24.0, "first column size becomes item height");
  assert_approx(
    second.bounds.y(),
    24.0,
    "second column offset does not include parent gap after transpose",
  );
  assert_approx(second.bounds.height(), 36.0, "second column size becomes item height");
}

#[test]
fn nested_subgrid_propagates_descendant_sizes() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(18.0)),
    GridTrack::Length(Length::px(26.0)),
  ];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(200.0));

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.grid_row_subgrid = true;
  outer_subgrid_style.grid_row_start = 1;
  outer_subgrid_style.grid_row_end = 3;

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.grid_row_subgrid = true;
  inner_subgrid_style.grid_row_start = 1;
  inner_subgrid_style.grid_row_end = 3;

  let mut inner_child1 = ComputedStyle::default();
  inner_child1.display = Display::Block;
  inner_child1.height = Some(Length::px(5.0));

  let mut inner_child2 = ComputedStyle::default();
  inner_child2.display = Display::Block;
  inner_child2.height = Some(Length::px(5.0));

  let grandchild1 =
    BoxNode::new_block(Arc::new(inner_child1), FormattingContextType::Block, vec![]);
  let grandchild2 =
    BoxNode::new_block(Arc::new(inner_child2), FormattingContextType::Block, vec![]);

  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![grandchild1, grandchild2],
  );

  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  assert_eq!(inner_fragment.children.len(), 2);

  let first = &inner_fragment.children[0];
  let second = &inner_fragment.children[1];

  assert_approx(first.bounds.y(), 0.0, "first nested row origin");
  assert_approx(second.bounds.y(), 18.0, "second nested row offset");
  assert!(
    first.bounds.height() <= 18.1,
    "first item fits within inherited track"
  );
  assert!(
    second.bounds.height() <= 26.1,
    "second item fits within inherited track"
  );
}

#[test]
fn nested_column_subgrid_respects_inherited_axes_overrides() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(42.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(75.0));

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.writing_mode = WritingMode::VerticalRl;
  outer_subgrid_style.grid_column_subgrid = true;
  outer_subgrid_style.grid_column_start = 1;
  outer_subgrid_style.grid_column_end = 3;
  outer_subgrid_style.justify_content = JustifyContent::Start;
  outer_subgrid_style.align_content = AlignContent::Start;

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.writing_mode = WritingMode::VerticalRl;
  inner_subgrid_style.grid_column_subgrid = true;
  inner_subgrid_style.grid_column_start = 1;
  inner_subgrid_style.grid_column_end = 3;
  inner_subgrid_style.justify_content = JustifyContent::Start;
  inner_subgrid_style.align_content = AlignContent::Start;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.justify_self = Some(AlignItems::Start);
  first_child.align_self = Some(AlignItems::Start);
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;
  first_child.height = Some(Length::px(6.0));

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.justify_self = Some(AlignItems::Start);
  second_child.align_self = Some(AlignItems::Start);
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;
  second_child.grid_row_start = 1;
  second_child.grid_row_end = 2;
  second_child.height = Some(Length::px(6.0));

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  assert_eq!(inner_fragment.children.len(), 2);

  let first = &inner_fragment.children[0];
  let second = &inner_fragment.children[1];
  assert_approx(
    first.bounds.x(),
    0.0,
    "first inherited column starts at origin on the physical X axis",
  );
  assert_approx(
    second.bounds.x(),
    33.0,
    "gap and first track size carry through nested subgrid on the physical X axis",
  );
}

#[test]
fn nested_subgrid_autoplacement_inherits_parent_tracks_for_descendants() {
  // Regression test for WPT `css/subgrid/subgrid-nested-writing-mode-001`:
  // A nested chain `container -> outer subgrid -> inner subgrid` where both subgrids are
  // auto-placed into the first column/row should still allow descendants to use the parent's
  // column tracks (and gap) instead of being clamped into a 1-track explicit grid.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(42.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(75.0));

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.writing_mode = WritingMode::VerticalRl;
  outer_subgrid_style.grid_column_subgrid = true;
  outer_subgrid_style.grid_row_subgrid = true;
  outer_subgrid_style.justify_content = JustifyContent::Start;
  outer_subgrid_style.align_content = AlignContent::Start;
  // Intentionally leave placement fully automatic (no grid_column_start/end).

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.writing_mode = WritingMode::VerticalRl;
  inner_subgrid_style.grid_column_subgrid = true;
  inner_subgrid_style.grid_row_subgrid = true;
  inner_subgrid_style.justify_content = JustifyContent::Start;
  inner_subgrid_style.align_content = AlignContent::Start;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.justify_self = Some(AlignItems::Start);
  first_child.align_self = Some(AlignItems::Start);
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;
  first_child.height = Some(Length::px(12.0));

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.justify_self = Some(AlignItems::Start);
  second_child.align_self = Some(AlignItems::Start);
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;
  second_child.grid_row_start = 1;
  second_child.grid_row_end = 2;
  second_child.height = Some(Length::px(12.0));

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  assert_eq!(inner_fragment.children.len(), 2);

  let first = &inner_fragment.children[0];
  let second = &inner_fragment.children[1];
  assert_approx(
    first.bounds.x(),
    0.0,
    "first inherited column starts at origin on the physical X axis",
  );
  assert_approx(
    second.bounds.x(),
    33.0,
    "gap and first track size carry through nested subgrid on the physical X axis",
  );
}

#[test]
fn subgrid_extends_named_lines() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(200.0));
  parent_style.grid_column_line_names = vec![vec!["a".into()], Vec::new(), vec!["b".into()]];
  let mut names: HashMap<String, Vec<usize>> = HashMap::new();
  names.insert("a".into(), vec![0]);
  names.insert("b".into(), vec![2]);
  parent_style.grid_column_names = names;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.subgrid_column_line_names = vec![
    vec!["a".into(), "sub-start".into()],
    vec!["c".into()],
    vec!["b".into()],
  ];
  subgrid_style.grid_column_line_names = subgrid_style.subgrid_column_line_names.clone();

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));
  child1_style.grid_column_raw = Some("a / c".into());

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(10.0));
  child2_style.grid_column_raw = Some("c / b".into());

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.x(), 0.0, "named start aligns to first line");
  assert_approx(first.bounds.width(), 20.0, "span to c covers first track");
  assert_approx(second.bounds.x(), 20.0, "c starts after first track");
  assert_approx(second.bounds.width(), 30.0, "c to b spans second track");
}

#[test]
fn subgrid_inherits_area_generated_line_names() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_areas = vec![
    vec![Some("nav".into()), Some("main".into())],
    vec![Some("nav".into()), Some("main".into())],
  ];
  parent_style.width = Some(Length::px(80.0));

  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut nav_child = ComputedStyle::default();
  nav_child.display = Display::Block;
  nav_child.height = Some(Length::px(10.0));
  nav_child.grid_column_raw = Some("nav-start / nav-end".into());
  nav_child.grid_row_start = 1;
  nav_child.grid_row_end = 2;

  let mut main_child = ComputedStyle::default();
  main_child.display = Display::Block;
  main_child.height = Some(Length::px(10.0));
  main_child.grid_column_raw = Some("main-start / main-end".into());
  main_child.grid_row_start = 2;
  main_child.grid_row_end = 3;

  let nav = BoxNode::new_block(Arc::new(nav_child), FormattingContextType::Block, vec![]);
  let main = BoxNode::new_block(Arc::new(main_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![nav, main],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(120.0, 120.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.children[0].bounds.x(),
    0.0,
    "nav starts on first track",
  );
  assert_approx(
    subgrid_fragment.children[0].bounds.width(),
    30.0,
    "nav spans nav column",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.x(),
    30.0,
    "main begins after nav end",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.width(),
    50.0,
    "main spans second column",
  );
}

#[test]
fn column_subgrid_respects_vertical_writing_mode() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(100.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(5.0));
  child1_style.grid_column_start = 1;
  child1_style.grid_column_end = 2;
  child1_style.grid_row_start = 1;
  child1_style.grid_row_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(5.0));
  child2_style.grid_column_start = 2;
  child2_style.grid_column_end = 3;
  child2_style.grid_row_start = 1;
  child2_style.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(
    first.bounds.y(),
    0.0,
    "first column maps to top in vertical mode",
  );
  assert_approx(
    second.bounds.y(),
    30.0,
    "second column starts after first track",
  );
  assert_approx(
    first.bounds.height(),
    5.0,
    "item keeps intrinsic height in column track",
  );
  assert_approx(
    second.bounds.height(),
    5.0,
    "item keeps intrinsic height in column track",
  );
}

#[test]
fn subgrid_tracks_transpose_when_writing_mode_differs() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(32.0)),
    GridTrack::Length(Length::px(48.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(8.0);
  parent_style.width = Some(Length::px(88.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  // Flip writing mode to ensure axes come from the parent grid, not the subgrid's own mode.
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  // Avoid `justify/align-content: normal` distributing free space into tracks, which would make the
  // observed inherited offsets depend on the available size rather than just (track + gap).
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.height = Some(Length::px(12.0));
  first_child.justify_self = Some(AlignItems::Start);
  first_child.align_self = Some(AlignItems::Start);
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.height = Some(Length::px(12.0));
  second_child.justify_self = Some(AlignItems::Start);
  second_child.align_self = Some(AlignItems::Start);
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.children[0].bounds.x(),
    0.0,
    "transposed row maps to x origin",
  );
  assert_approx(
    subgrid_fragment.children[0].bounds.width(),
    12.0,
    "row height becomes item width",
  );
  assert_approx(
    subgrid_fragment.children[0].bounds.y(),
    0.0,
    "first column starts at origin after transpose",
  );
  assert_approx(
    subgrid_fragment.children[0].bounds.height(),
    32.0,
    "first column size becomes item height",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.y(),
    32.0,
    "second column offset does not include parent gap after transpose",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.height(),
    48.0,
    "second column size becomes item height",
  );
}

#[test]
fn row_subgrid_inherits_block_axis_from_vertical_parent() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_row_gap = Length::px(10.0);
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::HorizontalTb;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  subgrid_style.grid_template_columns = vec![GridTrack::Auto];

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.height = Some(Length::px(6.0));
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.height = Some(Length::px(6.0));
  second_child.grid_row_start = 2;
  second_child.grid_row_end = 3;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(240.0, 120.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.bounds.width(),
    110.0,
    "subgrid inherits row sizing in the block axis",
  );
  assert_eq!(subgrid_fragment.children.len(), 2);
}

#[test]
fn row_subgrid_with_mismatched_writing_mode_transposes_tracks() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.width = Some(Length::px(120.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  subgrid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(15.0)),
    GridTrack::Length(Length::px(25.0)),
  ];
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;
  second_child.grid_row_start = 1;
  second_child.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(240.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.children[0].bounds.width(),
    40.0,
    "first row height becomes item width after transpose",
  );
  assert_approx(
    subgrid_fragment.children[0].bounds.height(),
    15.0,
    "first column width becomes item height after transpose",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.y(),
    15.0,
    "second column starts after the first track on the y-axis after transpose",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.height(),
    25.0,
    "second column width becomes item height after transpose",
  );
}

#[test]
fn subgrid_respects_its_own_justify_items() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(120.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.justify_items = AlignItems::Start;

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.font_size = 14.0;
  let text = BoxNode::new_text(Arc::new(text_style.clone()), "content".into());
  let inline = BoxNode::new_block(
    Arc::new(text_style),
    FormattingContextType::Inline,
    vec![text],
  );

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![inline],
  );

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(160.0, 160.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 1);
  let child_fragment = &subgrid_fragment.children[0];
  assert!(
    child_fragment.bounds.width() < 60.0,
    "child should shrink to its content instead of stretching to the track"
  );
  assert_approx(
    child_fragment.bounds.x(),
    0.0,
    "child stays at the start edge under justify-items:start",
  );
}

#[test]
fn subgrid_children_shape_parent_tracks_and_gaps() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.grid_row_gap = Length::px(6.0);
  parent_style.width = Some(Length::px(300.0));
  // Keep auto track sizes determined by their content contributions so we can assert the inherited
  // offsets and gaps precisely.
  parent_style.justify_content = JustifyContent::Start;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  // Keep alignment deterministic when inherited track sums differ from the subgrid's box size due
  // to writing-mode mismatch (axes swapped).
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;
  // Keep track alignment deterministic when inherited track sums differ from the subgrid's own box
  // size (a writing-mode mismatch remaps parent tracks onto different physical axes).
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut sub_child1 = ComputedStyle::default();
  sub_child1.display = Display::Block;
  sub_child1.width = Some(Length::px(40.0));
  sub_child1.height = Some(Length::px(20.0));
  sub_child1.grid_column_start = 1;
  sub_child1.grid_column_end = 2;
  sub_child1.grid_row_start = 1;
  sub_child1.grid_row_end = 2;

  let mut sub_child2 = ComputedStyle::default();
  sub_child2.display = Display::Block;
  sub_child2.width = Some(Length::px(70.0));
  sub_child2.height = Some(Length::px(30.0));
  sub_child2.grid_column_start = 2;
  sub_child2.grid_column_end = 3;
  sub_child2.grid_row_start = 2;
  sub_child2.grid_row_end = 3;

  let grand1 = BoxNode::new_block(Arc::new(sub_child1), FormattingContextType::Block, vec![]);
  let grand2 = BoxNode::new_block(Arc::new(sub_child2), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![grand1, grand2],
  );

  let mut marker_style = ComputedStyle::default();
  marker_style.display = Display::Block;
  marker_style.width = Some(Length::px(50.0));
  marker_style.height = Some(Length::px(5.0));
  marker_style.grid_column_start = 2;
  marker_style.grid_column_end = 3;
  marker_style.grid_row_start = 2;
  marker_style.grid_row_end = 3;
  let marker = BoxNode::new_block(Arc::new(marker_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid, marker],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(300.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let marker_fragment = &fragment.children[1];
  assert_approx(
    subgrid_fragment.children[0].bounds.width(),
    40.0,
    "first track matches grandchild",
  );
  assert_approx(
    subgrid_fragment.children[1].bounds.x(),
    45.0,
    "second track start accounts for gap",
  );
  assert_approx(
    marker_fragment.bounds.x(),
    subgrid_fragment.children[1].bounds.x(),
    "parent sibling aligns to inherited track",
  );
  assert_approx(
    marker_fragment.bounds.y(),
    26.0,
    "second row offset matches inherited sizing and gap",
  );
}

#[test]
fn subgrid_intrinsic_inline_size_accounts_for_children() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(6.0);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut wide = ComputedStyle::default();
  wide.display = Display::Block;
  wide.width = Some(Length::px(30.0));
  wide.grid_column_start = 1;
  wide.grid_column_end = 2;

  let mut tall = ComputedStyle::default();
  tall.display = Display::Block;
  tall.width = Some(Length::px(55.0));
  tall.grid_column_start = 2;
  tall.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(wide), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(tall), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let min_content = fc
    .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
    .expect("intrinsic size");
  assert_approx(
    min_content,
    91.0,
    "subgrid children contribute to min-content size",
  );
}

#[test]
fn rtl_direction_reverses_subgrid_inline_axis() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.direction = Direction::Rtl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(50.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.direction = Direction::Rtl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(5.0));
  child1_style.grid_column_start = 1;
  child1_style.grid_column_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(5.0));
  child2_style.grid_column_start = 2;
  child2_style.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(first.bounds.width(), 30.0, "first track width preserved");
  assert_approx(second.bounds.width(), 20.0, "second track width preserved");
  assert!(
    first.bounds.x() > second.bounds.x(),
    "rtl places the first track on the inline end",
  );
}

#[test]
fn subgrid_uses_local_direction_even_when_parent_differs() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.direction = Direction::Rtl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(22.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(50.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.direction = Direction::Ltr;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(18.0));
  child1_style.grid_column_start = 1;
  child1_style.grid_column_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(22.0));
  child2_style.grid_column_start = 2;
  child2_style.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(120.0, 120.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(first.bounds.width(), 28.0, "first track width");
  assert_approx(second.bounds.width(), 22.0, "second track width");
  assert!(
    first.bounds.x() < second.bounds.x(),
    "subgrid layout uses the subgrid's own direction"
  );
  assert_approx(
    second.bounds.x() - first.bounds.x(),
    28.0,
    "tracks remain adjacent in ltr ordering",
  );
}

#[test]
fn row_subgrid_respects_local_direction_for_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Length(Length::px(60.0))];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.width = Some(Length::px(60.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  subgrid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
  ];
  subgrid_style.direction = Direction::Rtl;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.height = Some(Length::px(5.0));
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.height = Some(Length::px(5.0));
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;
  second_child.grid_row_start = 1;
  second_child.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(120.0, 120.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert!(
    first.bounds.x() > second.bounds.x(),
    "rtl direction on row subgrid should mirror the inline axis"
  );
  assert_approx(first.bounds.width(), 20.0, "first column width preserved");
  assert_approx(second.bounds.width(), 30.0, "second column width preserved");
}

#[test]
fn subgrid_writing_mode_mismatch_rtl_uses_subgrid_inline_axis_vertical() {
  let col1 = 20.0;
  let col2 = 30.0;
  let row1 = 40.0;
  let row2 = 50.0;
  let item = 10.0;

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::HorizontalTb;
  parent_style.direction = Direction::Rtl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(col1)),
    GridTrack::Length(Length::px(col2)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(row1)),
    GridTrack::Length(Length::px(row2)),
  ];
  parent_style.width = Some(Length::px(col1 + col2));
  parent_style.height = Some(Length::px(row1 + row2));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.direction = Direction::Rtl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(item));
  first_style.height = Some(Length::px(item));
  first_style.justify_self = Some(AlignItems::Start);
  first_style.align_self = Some(AlignItems::Start);
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(item));
  second_style.height = Some(Length::px(item));
  second_style.justify_self = Some(AlignItems::Start);
  second_style.align_self = Some(AlignItems::Start);
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  second_style.grid_row_start = 1;
  second_style.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.bounds.width(),
    col1 + col2,
    "subgrid width inherits parent columns",
  );
  assert_approx(
    subgrid_fragment.bounds.height(),
    row1 + row2,
    "subgrid height inherits parent rows",
  );

  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  // The subgrid has a different writing mode than its parent. Subgrid indexing rules use the
  // subgrid's own axes, so here its inline axis is vertical. With `direction: rtl` that means the
  // inline-start edge is at the bottom (physical Y).
  assert_approx(
    first.bounds.x(),
    col1 + col2 - item,
    "block-start is right in vertical-rl",
  );
  assert_approx(
    second.bounds.x(),
    col1 + col2 - item,
    "block-start is right in vertical-rl",
  );
  assert_approx(
    first.bounds.y(),
    row1 + row2 - item,
    "column 1 aligns to inline-start (bottom) in rtl vertical mode",
  );
  assert_approx(
    second.bounds.y(),
    row1 + row2 - col1 - item,
    "column 2 aligns just above column 1 (after the first inherited track)",
  );
  assert!(
    first.bounds.y() > second.bounds.y(),
    "rtl mirrors column ordering on the Y axis"
  );
}

#[test]
fn subgrid_writing_mode_mismatch_rtl_uses_subgrid_inline_axis_horizontal() {
  let col1 = 20.0;
  let col2 = 30.0;
  let row1 = 40.0;
  let row2 = 50.0;
  let item = 10.0;

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.direction = Direction::Rtl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(col1)),
    GridTrack::Length(Length::px(col2)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(row1)),
    GridTrack::Length(Length::px(row2)),
  ];
  parent_style.width = Some(Length::px(row1 + row2));
  parent_style.height = Some(Length::px(col1 + col2));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::HorizontalTb;
  subgrid_style.direction = Direction::Rtl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(item));
  first_style.height = Some(Length::px(item));
  first_style.justify_self = Some(AlignItems::Start);
  first_style.align_self = Some(AlignItems::Start);
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(item));
  second_style.height = Some(Length::px(item));
  second_style.justify_self = Some(AlignItems::Start);
  second_style.align_self = Some(AlignItems::Start);
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  second_style.grid_row_start = 1;
  second_style.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.bounds.width(),
    row1 + row2,
    "subgrid width uses parent's vertical-mode row tracks",
  );
  assert_approx(
    subgrid_fragment.bounds.height(),
    col1 + col2,
    "subgrid height uses parent's vertical-mode column tracks",
  );

  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  // Here the subgrid flips back to horizontal writing mode, so its inline axis is physical X.
  // `direction: rtl` must therefore mirror the column ordering on physical X even though its parent
  // has a vertical inline axis.
  assert_approx(
    first.bounds.y(),
    0.0,
    "block-start is top in horizontal writing mode",
  );
  assert_approx(
    second.bounds.y(),
    0.0,
    "block-start is top in horizontal writing mode",
  );
  assert_approx(
    first.bounds.x(),
    row1 + row2 - item,
    "column 1 aligns to inline-start (right) in rtl horizontal mode",
  );
  assert_approx(
    second.bounds.x(),
    row1 + row2 - col1 - item,
    "column 2 aligns just left of column 1 (after the first inherited track)",
  );
  assert!(
    first.bounds.x() > second.bounds.x(),
    "rtl mirrors column ordering on the X axis"
  );
}

// Per CSS Writing Modes, `direction` affects the inline base direction even when the inline axis is
// vertical. For vertical writing modes this means `direction: rtl` flips inline-start/inline-end on
// the physical Y axis, so grid "columns" (the inline axis) flow bottom-to-top.
fn assert_vertical_writing_mode_direction_rtl_mirrors_inline_axis(writing_mode: WritingMode) {
  let col1 = 20.0;
  let col2 = 30.0;
  let row = 40.0;
  let item = 10.0;

  // Grid container case (detailed track info available).
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.writing_mode = writing_mode;
  grid_style.direction = Direction::Rtl;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(col1)),
    GridTrack::Length(Length::px(col2)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(row))];
  grid_style.width = Some(Length::px(row));
  grid_style.height = Some(Length::px(col1 + col2));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(item));
  first_style.height = Some(Length::px(item));
  first_style.justify_self = Some(AlignItems::Start);
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(item));
  second_style.height = Some(Length::px(item));
  second_style.justify_self = Some(AlignItems::Start);
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  second_style.grid_row_start = 1;
  second_style.grid_row_end = 2;

  let child1 = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let first = &fragment.children[0];
  let second = &fragment.children[1];
  assert!(
    first.bounds.y() > second.bounds.y(),
    "rtl should reverse the inline axis when it is vertical",
  );
  assert_approx(
    first.bounds.y(),
    col1 + col2 - item,
    "column 1 is at the inline-start (bottom) edge",
  );
  assert_approx(
    second.bounds.y(),
    col2 - item,
    "column 2 is immediately above column 1",
  );

  // Column subgrid case (subgrid track offsets derived from ancestor).
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = writing_mode;
  parent_style.direction = Direction::Rtl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(col1)),
    GridTrack::Length(Length::px(col2)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(row))];
  parent_style.width = Some(Length::px(row));
  parent_style.height = Some(Length::px(col1 + col2));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = writing_mode;
  subgrid_style.direction = Direction::Rtl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 2;

  let mut sub_first = ComputedStyle::default();
  sub_first.display = Display::Block;
  sub_first.width = Some(Length::px(item));
  sub_first.height = Some(Length::px(item));
  sub_first.justify_self = Some(AlignItems::Start);
  sub_first.grid_column_start = 1;
  sub_first.grid_column_end = 2;
  sub_first.grid_row_start = 1;
  sub_first.grid_row_end = 2;

  let mut sub_second = ComputedStyle::default();
  sub_second.display = Display::Block;
  sub_second.width = Some(Length::px(item));
  sub_second.height = Some(Length::px(item));
  sub_second.justify_self = Some(AlignItems::Start);
  sub_second.grid_column_start = 2;
  sub_second.grid_column_end = 3;
  sub_second.grid_row_start = 1;
  sub_second.grid_row_end = 2;

  let sub_child1 = BoxNode::new_block(Arc::new(sub_first), FormattingContextType::Block, vec![]);
  let sub_child2 = BoxNode::new_block(Arc::new(sub_second), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![sub_child1, sub_child2],
  );
  let parent = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");
  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert!(
    first.bounds.y() > second.bounds.y(),
    "rtl should reverse the inline axis inside a vertical-writing-mode subgrid",
  );
  assert_approx(
    first.bounds.y(),
    col1 + col2 - item,
    "subgrid column 1 aligns to inline-start (bottom)",
  );
  assert_approx(
    second.bounds.y(),
    col2 - item,
    "subgrid column 2 aligns above column 1",
  );
}

#[test]
fn vertical_rl_direction_rtl_mirrors_inline_axis() {
  assert_vertical_writing_mode_direction_rtl_mirrors_inline_axis(WritingMode::VerticalRl);
}

#[test]
fn vertical_lr_direction_rtl_mirrors_inline_axis() {
  assert_vertical_writing_mode_direction_rtl_mirrors_inline_axis(WritingMode::VerticalLr);
}

#[test]
fn sideways_rl_direction_rtl_mirrors_inline_axis() {
  assert_vertical_writing_mode_direction_rtl_mirrors_inline_axis(WritingMode::SidewaysRl);
}

#[test]
fn subgrid_inherits_named_lines_with_offset() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(25.0)),
    GridTrack::Length(Length::px(35.0)),
    GridTrack::Length(Length::px(45.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(105.0));
  parent_style.grid_column_line_names = vec![
    vec!["one".into()],
    vec!["two".into()],
    vec!["three".into()],
    vec!["four".into()],
  ];
  let mut names: HashMap<String, Vec<usize>> = HashMap::new();
  names.insert("one".into(), vec![0]);
  names.insert("two".into(), vec![1]);
  names.insert("three".into(), vec![2]);
  names.insert("four".into(), vec![3]);
  parent_style.grid_column_names = names;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 4;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.height = Some(Length::px(10.0));
  first_child.grid_column_raw = Some("two / three".into());

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.height = Some(Length::px(10.0));
  second_child.grid_column_raw = Some("three / four".into());

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let child0 = &subgrid_fragment.children[0];
  let child1 = &subgrid_fragment.children[1];
  assert_approx(
    subgrid_fragment.bounds.x(),
    25.0,
    "subgrid starts after the first parent track",
  );
  assert_approx(
    child0.bounds.x(),
    0.0,
    "first child starts at inherited line two",
  );
  assert_approx(
    child0.bounds.width(),
    35.0,
    "first child spans the second parent track",
  );
  assert_approx(
    child1.bounds.x(),
    35.0,
    "second child begins after the inherited track",
  );
  assert_approx(
    child1.bounds.width(),
    45.0,
    "second child spans the third parent track",
  );
}

#[test]
fn subgrid_inherits_area_line_names_with_offset() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(25.0)),
    GridTrack::Length(Length::px(35.0)),
    GridTrack::Length(Length::px(45.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_areas = vec![
    vec![
      Some("left".into()),
      Some("main".into()),
      Some("right".into()),
    ],
    vec![
      Some("left".into()),
      Some("main".into()),
      Some("right".into()),
    ],
  ];
  parent_style.width = Some(Length::px(105.0));
  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 4;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.height = Some(Length::px(10.0));
  first_child.grid_column_raw = Some("main-start / main-end".into());

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.height = Some(Length::px(10.0));
  second_child.grid_column_raw = Some("right-start / right-end".into());

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(
    subgrid_fragment.bounds.x(),
    25.0,
    "subgrid starts after the left area track",
  );
  assert_approx(first.bounds.x(), 0.0, "main-start maps to subgrid start");
  assert_approx(
    first.bounds.width(),
    35.0,
    "main area width preserved inside subgrid",
  );
  assert_approx(second.bounds.x(), 35.0, "right area starts after main");
  assert_approx(
    second.bounds.width(),
    45.0,
    "right area width preserved inside subgrid",
  );
}

#[test]
fn column_subgrid_inherits_gap_in_vertical_parent() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(18.0)),
    GridTrack::Length(Length::px(22.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(6.0);
  parent_style.width = Some(Length::px(60.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(4.0));
  child1_style.grid_column_start = 1;
  child1_style.grid_column_end = 2;

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(4.0));
  child2_style.grid_column_start = 2;
  child2_style.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(
    first.bounds.y(),
    0.0,
    "first column origin follows parent track",
  );
  assert_approx(
    subgrid_fragment.bounds.height(),
    46.0,
    "subgrid height matches inherited column tracks",
  );
  assert_approx(
    second.bounds.y(),
    24.0,
    "gap between inherited column tracks is preserved",
  );
}

#[test]
fn subgrid_max_content_inline_size_accounts_for_children() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(4.0);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut wide = ComputedStyle::default();
  wide.display = Display::Block;
  wide.width = Some(Length::px(25.0));
  wide.grid_column_start = 1;
  wide.grid_column_end = 2;

  let mut tall = ComputedStyle::default();
  tall.display = Display::Block;
  tall.width = Some(Length::px(40.0));
  tall.grid_column_start = 2;
  tall.grid_column_end = 3;

  let child1 = BoxNode::new_block(Arc::new(wide), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(tall), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let max_content = fc
    .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic size");
  assert_approx(
    max_content,
    69.0,
    "max-content spans inherited tracks and gaps",
  );
}

#[test]
fn column_subgrid_height_contribution_requires_inherited_track_sizes_during_measurement() {
  // Regression: measuring a subgrid item (RunMode::ComputeSize inside Taffy) must account for
  // inherited track sizes. This scenario makes the subgrid item's height depend on the width of an
  // inherited column via aspect-ratio, so incorrect subgrid measurement causes the parent's auto
  // row sizing to be wrong.
  //
  // Layout structure:
  // outer grid (100px wide) -> inner grid with columns (30px, 70px) -> column-subgrid spanning both columns.
  // The subgrid contains a single item in its second column. The item has `aspect-ratio: 2` and is
  // left as `width: auto` so it stretches to the column width (70px). Expected height is 70/2 =
  // 35px.
  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  outer_style.grid_template_rows = vec![GridTrack::Auto];

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(70.0)),
  ];
  inner_style.grid_template_rows = vec![GridTrack::Auto];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  // Provide an explicit (non-inherited) track list so that, without overrides, the subgrid would
  // size its columns differently. With correct subgrid overrides it should use the parent's (30,70)
  // track sizes instead.
  subgrid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  subgrid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.aspect_ratio = AspectRatio::Ratio(2.0);
  item_style.grid_column_start = 2;
  item_style.grid_column_end = 3;
  item_style.grid_row_start = 2;
  item_style.grid_row_end = 3;

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![item],
  );
  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&outer, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  assert_approx(
    fragment.bounds.height(),
    35.0,
    "outer grid height reflects subgrid's measured aspect-ratio item",
  );
}

#[test]
fn subgrid_inherits_tracks_on_both_axes_across_writing_modes() {
  let col1 = 40.0;
  let col2 = 50.0;
  let row1 = 25.0;
  let row2 = 35.0;
  let col_gap = 6.0;
  let row_gap = 4.0;
  let parent_width = col1 + col_gap + col2;
  let parent_height = row1 + row_gap + row2;

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(col1)),
    GridTrack::Length(Length::px(col2)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(row1)),
    GridTrack::Length(Length::px(row2)),
  ];
  parent_style.grid_column_gap = Length::px(col_gap);
  parent_style.grid_row_gap = Length::px(row_gap);
  parent_style.width = Some(Length::px(parent_width));
  parent_style.height = Some(Length::px(parent_height));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  // Prevent `justify/align-content: normal` distributing free space, making the expected offsets
  // dependent on the available size rather than inherited track lists.
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;
  // Ensure auto-sized children fill their grid areas so track sizes are observable via the item
  // fragment bounds.
  subgrid_style.justify_items = AlignItems::Stretch;
  subgrid_style.align_items = AlignItems::Stretch;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.grid_column_start = 1;
  first_child.grid_column_end = 2;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.grid_column_start = 2;
  second_child.grid_column_end = 3;
  second_child.grid_row_start = 2;
  second_child.grid_row_end = 3;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_approx(
    subgrid_fragment.bounds.width(),
    parent_width,
    "subgrid width matches parent tracks",
  );
  assert_approx(
    subgrid_fragment.bounds.height(),
    parent_height,
    "subgrid height matches parent tracks",
  );

  let row1_x = parent_width - row1;
  let row2_x = row1_x - row_gap - row2;
  // When the subgrid establishes an orthogonal writing mode, inherited column gutters stay on the
  // parent's physical X axis and do not transpose into the subgrid's physical Y axis. (Matches WPT
  // `css/subgrid/subgrid-writing-mode-001`.)
  let col2_y = col1;

  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];
  assert_approx(
    first.bounds.y(),
    0.0,
    "first column maps onto the vertical axis",
  );
  assert_approx(
    first.bounds.height(),
    col1,
    "first column size inherited from the parent",
  );
  assert_approx(
    first.bounds.x(),
    row1_x,
    "first row aligns to block-start (right) in vertical-rl",
  );
  assert_approx(
    first.bounds.width(),
    row1,
    "first row size inherited from the parent",
  );

  assert_approx(
    second.bounds.y(),
    col2_y,
    "second column offset ignores the parent's column gap after writing-mode transpose",
  );
  assert_approx(
    second.bounds.height(),
    col2,
    "second column size inherited from the parent",
  );
  assert_approx(
    second.bounds.x(),
    row2_x,
    "second row offset includes inherited gap",
  );
  assert_approx(
    second.bounds.width(),
    row2,
    "second row size inherited from the parent",
  );
}

#[test]
fn subgrid_named_lines_survive_writing_mode_transpose() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(25.0)),
    GridTrack::Length(Length::px(35.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(65.0));
  parent_style.grid_column_line_names =
    vec![vec!["start".into()], vec!["mid".into()], vec!["end".into()]];
  let mut names: HashMap<String, Vec<usize>> = HashMap::new();
  names.insert("start".into(), vec![0]);
  names.insert("mid".into(), vec![1]);
  names.insert("end".into(), vec![2]);
  parent_style.grid_column_names = names;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.grid_column_raw = Some("start / mid".into());

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.grid_column_raw = Some("mid / end".into());

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.height(), 25.0, "start/mid span first column");
  assert_approx(first.bounds.y(), 0.0, "first column starts at origin");
  assert_approx(second.bounds.height(), 35.0, "mid/end span second column");
  assert_approx(
    second.bounds.y(),
    25.0,
    "column gap does not transpose onto the physical Y axis when writing-modes differ",
  );
}

#[test]
fn subgrid_inherits_area_lines_when_axes_differ() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(4.0);
  parent_style.grid_template_areas = vec![
    vec![Some("nav".into()), Some("main".into())],
    vec![Some("nav".into()), Some("main".into())],
  ];
  parent_style.width = Some(Length::px(74.0));
  synthesize_area_line_names(&mut parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.writing_mode = WritingMode::HorizontalTb;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut nav_child = ComputedStyle::default();
  nav_child.display = Display::Block;
  nav_child.grid_column_raw = Some("nav-start / nav-end".into());
  nav_child.grid_row_start = 1;
  nav_child.grid_row_end = 2;

  let mut main_child = ComputedStyle::default();
  main_child.display = Display::Block;
  main_child.grid_column_raw = Some("main-start / main-end".into());
  main_child.grid_row_start = 2;
  main_child.grid_row_end = 3;

  let nav = BoxNode::new_block(Arc::new(nav_child), FormattingContextType::Block, vec![]);
  let main = BoxNode::new_block(Arc::new(main_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![nav, main],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(120.0, 120.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let nav_fragment = &subgrid_fragment.children[0];
  let main_fragment = &subgrid_fragment.children[1];

  assert_approx(
    nav_fragment.bounds.x(),
    0.0,
    "nav stays in the first inherited track",
  );
  assert_approx(
    nav_fragment.bounds.width(),
    30.0,
    "nav spans first inherited column",
  );
  assert_approx(
    main_fragment.bounds.x(),
    34.0,
    "gap and track offset preserved for main",
  );
  assert_approx(
    main_fragment.bounds.width(),
    40.0,
    "main spans the second inherited column",
  );
}

#[test]
fn row_subgrid_inherits_tracks_from_vertical_parent() {
  let row1 = 40.0;
  let row2 = 60.0;
  let row_gap = 8.0;
  let parent_width = row1 + row_gap + row2;

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(row1)),
    GridTrack::Length(Length::px(row2)),
  ];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_row_gap = Length::px(row_gap);
  parent_style.width = Some(Length::px(parent_width));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;
  subgrid_style.writing_mode = WritingMode::HorizontalTb;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;
  subgrid_style.justify_items = AlignItems::Stretch;
  subgrid_style.align_items = AlignItems::Stretch;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.grid_row_start = 1;
  first_child.grid_row_end = 2;

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.grid_row_start = 2;
  second_child.grid_row_end = 3;

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(
    subgrid_fragment.bounds.width(),
    parent_width,
    "subgrid width matches inherited rows",
  );

  // The subgrid's own writing mode is horizontal, so its row axis is physical Y even though its
  // parent has a vertical writing mode (axes swapped). With correct axis remapping, the parent's
  // row tracks should be inherited onto the subgrid's physical Y axis.
  assert_approx(
    first.bounds.y(),
    0.0,
    "first inherited row starts at block-start (top) in horizontal writing mode",
  );
  assert_approx(first.bounds.height(), row1, "first inherited row size");
  assert_approx(
    second.bounds.y(),
    row1 + row_gap,
    "second inherited row offset includes the inherited gap",
  );
  assert_approx(second.bounds.height(), row2, "second inherited row size");
}

#[test]
fn subgrid_named_lines_resolve_in_vertical_mode() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(25.0)),
    GridTrack::Length(Length::px(35.0)),
    GridTrack::Length(Length::px(45.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(105.0));
  parent_style.grid_column_line_names = vec![
    vec!["one".into()],
    vec!["two".into()],
    vec!["three".into()],
    vec!["four".into()],
  ];
  let mut names: HashMap<String, Vec<usize>> = HashMap::new();
  names.insert("one".into(), vec![0]);
  names.insert("two".into(), vec![1]);
  names.insert("three".into(), vec![2]);
  names.insert("four".into(), vec![3]);
  parent_style.grid_column_names = names;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 4;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut first_child = ComputedStyle::default();
  first_child.display = Display::Block;
  first_child.grid_column_raw = Some("two / three".into());

  let mut second_child = ComputedStyle::default();
  second_child.display = Display::Block;
  second_child.grid_column_raw = Some("three / four".into());

  let child1 = BoxNode::new_block(Arc::new(first_child), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(second_child), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  let child0 = &subgrid_fragment.children[0];
  let child1 = &subgrid_fragment.children[1];
  assert_approx(
    subgrid_fragment.bounds.y(),
    25.0,
    "subgrid starts after the first parent track",
  );
  assert_approx(
    child0.bounds.y(),
    0.0,
    "first named slice starts at inherited line two",
  );
  assert_approx(
    child0.bounds.height(),
    35.0,
    "first named span inherits second track size",
  );
  assert_approx(
    child1.bounds.y(),
    35.0,
    "second named slice begins after inherited track",
  );
  assert_approx(
    child1.bounds.height(),
    45.0,
    "second named span inherits third track size",
  );
}

#[test]
fn nested_subgrids_with_writing_mode_inherit_parent_tracks_for_auto_span() {
  // Mirrors WPT `css/subgrid/subgrid-nested-writing-mode-001`.
  //
  // The nested subgrids (`outer` and `inner`) are auto-placed (no explicit grid-column/row
  // placement). They should still inherit *all* parent tracks so the leaf items can land in
  // inherited columns 1 and 2 even though the subgrids specify a mismatched `writing-mode`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(42.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  container_style.grid_column_gap = Length::px(5.0);
  container_style.grid_column_gap_is_normal = false;
  // Style parsing normally materializes a (tracks + 1)-length line-name vector even when no names
  // are authored. Nested-subgrid auto-span relies on the parent line count being available through
  // this vector when the immediate parent is itself a subgrid (and thus has no explicit track
  // list).
  container_style.grid_column_line_names = vec![Vec::new(), Vec::new(), Vec::new()];
  container_style.grid_row_line_names = vec![Vec::new(), Vec::new()];
  container_style.width = Some(Length::px(75.0));
  container_style.padding_left = Length::px(4.0);
  container_style.padding_right = Length::px(4.0);
  container_style.padding_top = Length::px(4.0);
  container_style.padding_bottom = Length::px(4.0);

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_column_subgrid = true;
  outer_style.grid_row_subgrid = true;
  // A plain `subgrid` track list omits the optional line-name-list. The style parser represents
  // that omission as an empty Vec (as opposed to an explicit empty list `subgrid []`, which would
  // be `[[]]`).
  outer_style.subgrid_column_line_names = Vec::new();
  outer_style.subgrid_row_line_names = Vec::new();
  outer_style.writing_mode = WritingMode::VerticalRl;
  outer_style.grid_column_gap = container_style.grid_column_gap;
  outer_style.grid_column_gap_is_normal = container_style.grid_column_gap_is_normal;
  outer_style.justify_content = JustifyContent::Start;
  outer_style.align_content = AlignContent::Start;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_column_subgrid = true;
  inner_style.grid_row_subgrid = true;
  inner_style.subgrid_column_line_names = Vec::new();
  inner_style.subgrid_row_line_names = Vec::new();
  inner_style.writing_mode = WritingMode::VerticalRl;
  inner_style.grid_column_gap = container_style.grid_column_gap;
  inner_style.grid_column_gap_is_normal = container_style.grid_column_gap_is_normal;
  inner_style.justify_content = JustifyContent::Start;
  inner_style.align_content = AlignContent::Start;

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.justify_self = Some(AlignItems::Start);
  a_style.align_self = Some(AlignItems::Start);
  a_style.height = Some(Length::px(12.0));
  a_style.grid_column_start = 1;
  a_style.grid_column_end = 2;
  a_style.grid_row_start = 1;
  a_style.grid_row_end = 2;

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  b_style.justify_self = Some(AlignItems::Start);
  b_style.align_self = Some(AlignItems::Start);
  b_style.height = Some(Length::px(12.0));
  b_style.grid_column_start = 2;
  b_style.grid_column_end = 3;
  b_style.grid_row_start = 1;
  b_style.grid_row_end = 2;

  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);
  let b = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);

  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![a, b],
  );
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner],
  );
  let grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![outer],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  let a_fragment = &inner_fragment.children[0];
  let b_fragment = &inner_fragment.children[1];

  // Even though `outer`/`inner` set `writing-mode: vertical-rl`, nested subgrids should keep the
  // containing grid's axis mapping for their track inheritance. (See WPT
  // `css/subgrid/subgrid-nested-writing-mode-001`.)
  assert_approx(a_fragment.bounds.x(), 0.0, "first item starts in column 1");
  assert_approx(
    b_fragment.bounds.x(),
    33.0,
    "second item starts in column 2 (+ gap)",
  );
}

#[test]
fn abspos_named_lines_resolve_in_subgrid_with_writing_mode_mismatch() {
  // Regression: absolute-positioned items are excluded from the Taffy tree, so their grid-based
  // static positions must resolve named lines using reconstructed (inherited) line-name vectors.
  // Ensure this still works when the subgrid overrides `writing-mode`.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.width = Some(Length::px(100.0));
  parent_style.grid_column_line_names = vec![
    vec!["one".into()],
    vec!["two".into()],
    vec!["three".into()],
    vec!["four".into()],
  ];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 2;
  subgrid_style.grid_column_end = 4;
  subgrid_style.justify_content = JustifyContent::Start;
  subgrid_style.align_content = AlignContent::Start;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("three / four".into());

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.y(),
    30.0,
    "named line placement resolves to the second inherited track on the physical Y axis",
  );
}

#[test]
fn abspos_named_lines_resolve_through_nested_subgrids_with_writing_mode_mismatch() {
  // Like the single-subgrid case above, but ensure line-name inheritance works across a subgrid
  // chain (subgrid -> subgrid -> grid) in a vertical writing mode (axes swapped).
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.writing_mode = WritingMode::VerticalRl;
  // In vertical writing modes, CSS grid columns map to physical Y. Choose sizes that make the
  // expected offsets easy to assert.
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  // CSS grid rows map to physical X.
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
  parent_style.width = Some(Length::px(100.0));
  parent_style.height = Some(Length::px(90.0));
  parent_style.grid_column_line_names = vec![
    vec!["one".into()],
    vec!["two".into()],
    vec!["three".into()],
    vec!["four".into()],
  ];

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.writing_mode = WritingMode::HorizontalTb;
  outer_subgrid_style.grid_column_subgrid = true;
  outer_subgrid_style.grid_column_start = 2;
  outer_subgrid_style.grid_column_end = 4;
  outer_subgrid_style.grid_row_start = 1;
  outer_subgrid_style.grid_row_end = 2;
  outer_subgrid_style.justify_content = JustifyContent::Start;
  outer_subgrid_style.align_content = AlignContent::Start;

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.writing_mode = WritingMode::HorizontalTb;
  inner_subgrid_style.grid_column_subgrid = true;
  inner_subgrid_style.grid_column_start = 1;
  inner_subgrid_style.grid_column_end = 3;
  inner_subgrid_style.grid_row_start = 1;
  inner_subgrid_style.grid_row_end = 2;
  inner_subgrid_style.justify_content = JustifyContent::Start;
  inner_subgrid_style.align_content = AlignContent::Start;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  // Outer subgrid spans parent lines 2-4, so "three / four" should select the second inherited
  // track within the nested subgrid chain.
  abs_style.grid_column_raw = Some("three / four".into());

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.x(),
    30.0,
    "named line placement resolves to the second inherited column track on the physical X axis",
  );
}

#[test]
fn abspos_row_subgrid_respects_local_direction_for_columns_through_subgrid_chain() {
  // Regression: when reconstructing grid-based static positions for out-of-flow items, subgrids
  // should use the same effective axis style as grid layout. In particular, `direction` should
  // come from the grid container's computed style for the axis being placed (mirroring should not
  // leak in from ancestors when the axis is local).
  //
  // This case creates a subgrid chain:
  //   root grid (direction: rtl) -> column-subgrid -> row-subgrid (direction: ltr)
  //
  // The nested row-subgrid defines its own columns, so it must keep its local `direction:ltr`
  // for column placement even though the ancestor grid is `direction: rtl`.
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Grid;
  root_style.position = Position::Relative;
  root_style.direction = Direction::Rtl;
  root_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
  root_style.grid_template_rows = vec![GridTrack::Length(Length::px(10.0))];
  root_style.width = Some(Length::px(50.0));
  root_style.height = Some(Length::px(10.0));

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_column_subgrid = true;
  outer_style.grid_column_start = 1;
  outer_style.grid_column_end = 2;
  outer_style.grid_row_start = 1;
  outer_style.grid_row_end = 2;
  outer_style.grid_template_rows = vec![GridTrack::Length(Length::px(10.0))];
  // Intentionally choose the opposite direction to ensure the nested row-subgrid keeps its own
  // column direction rather than inheriting RTL mirroring from the ancestor.
  outer_style.direction = Direction::Ltr;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_row_subgrid = true;
  inner_style.grid_row_start = 1;
  inner_style.grid_row_end = 2;
  // Columns are local to this grid (not subgridded), so direction must be honored locally.
  inner_style.direction = Direction::Ltr;
  inner_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
  ];

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  // Place in the *second* column so incorrect RTL mirroring is observable (it would move the item
  // to x=0 instead of x=20).
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner],
  );
  let grid = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Grid,
    vec![outer],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  let abs_fragment = inner_fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.x(),
    20.0,
    "row-subgrid should keep local direction for column placement (no RTL mirroring)",
  );
}
