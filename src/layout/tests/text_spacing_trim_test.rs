use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_first_block_with_line_children<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_first_block_with_line_children(child) {
      return Some(found);
    }
  }
  None
}

fn line_children(block: &FragmentNode) -> Vec<FragmentNode> {
  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .cloned()
    .collect()
}

fn first_text_x_in_line(line: &FragmentNode) -> Option<f32> {
  fn walk(node: &FragmentNode, offset_x: f32) -> Option<f32> {
    let offset_x = offset_x + node.bounds.x();
    if matches!(node.content, FragmentContent::Text { .. }) {
      return Some(offset_x);
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, offset_x) {
        return Some(found);
      }
    }
    None
  }

  for child in line.children.iter() {
    if let Some(found) = walk(child, 0.0) {
      return Some(found);
    }
  }
  None
}

fn text_x_in_line(line: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(node: &FragmentNode, needle: &str, offset_x: f32) -> Option<f32> {
    let offset_x = offset_x + node.bounds.x();
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return Some(offset_x);
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, needle, offset_x) {
        return Some(found);
      }
    }
    None
  }

  for child in line.children.iter() {
    if let Some(found) = walk(child, needle, 0.0) {
      return Some(found);
    }
  }
  None
}

fn inline_x_in_line_containing_text(line: &FragmentNode, needle: &str) -> Option<f32> {
  fn contains_text(node: &FragmentNode, needle: &str) -> bool {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return true;
      }
    }
    node.children.iter().any(|child| contains_text(child, needle))
  }

  fn walk(node: &FragmentNode, needle: &str, offset_x: f32) -> Option<f32> {
    let offset_x = offset_x + node.bounds.x();
    if matches!(node.content, FragmentContent::Inline { .. }) && contains_text(node, needle) {
      return Some(offset_x);
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, needle, offset_x) {
        return Some(found);
      }
    }
    None
  }

  for child in line.children.iter() {
    if let Some(found) = walk(child, needle, 0.0) {
      return Some(found);
    }
  }
  None
}

fn content_max_x_in_line(line: &FragmentNode) -> f32 {
  fn walk(node: &FragmentNode, offset_x: f32, max_x: &mut f32) {
    let offset_x = offset_x + node.bounds.x();
    *max_x = max_x.max(offset_x + node.bounds.width());
    for child in node.children.iter() {
      walk(child, offset_x, max_x);
    }
  }

  let mut max_x = 0.0;
  for child in line.children.iter() {
    walk(child, 0.0, &mut max_x);
  }
  max_x
}

fn layout_lines_with_box_style(box_style: &str, inner_html: &str) -> Vec<FragmentNode> {
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; }}
        .box {{
          font-family: 'DejaVu Sans', sans-serif;
          font-size: 24px;
          line-height: 1;
          {box_style}
        }}
      </style>
      <div class="box">{inner_html}</div>
    "#
  );

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 400, 200)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");
  line_children(block)
}

fn layout_lines(text_spacing_trim: &str, text_align: &str) -> Vec<FragmentNode> {
  layout_lines_with_box_style(
    &format!(
      "width: 60px; word-break: break-all; text-align: {text_align}; text-spacing-trim: {text_spacing_trim};"
    ),
    "「HELLOWORLD」",
  )
}

#[test]
fn text_spacing_trim_hangs_fullwidth_punctuation() {
  // Compare default behavior vs `trim-both`.
  let space_all_left = layout_lines("space-all", "left");
  let trim_both_left = layout_lines("trim-both", "left");

  let space_first_x = first_text_x_in_line(&space_all_left[0]).expect("first text x (space-all)");
  let trim_first_x = first_text_x_in_line(&trim_both_left[0]).expect("first text x (trim-both)");

  assert!(
    trim_first_x < space_first_x - 0.1,
    "expected trim-both to shift the leading opening punctuation left: space-all x={space_first_x:.3} trim-both x={trim_first_x:.3}"
  );
  assert!(
    trim_first_x < 0.0,
    "expected leading punctuation to hang into the start edge (negative x), got {trim_first_x:.3}"
  );

  // Use `text-align: right` so the trimmed closing punctuation hangs past the line end.
  let space_all_right = layout_lines("space-all", "right");
  let trim_both_right = layout_lines("trim-both", "right");

  let last_space_line = space_all_right.last().expect("space-all last line");
  let last_trim_line = trim_both_right.last().expect("trim-both last line");

  let space_bbox = last_space_line.bounding_box();
  let trim_bbox = last_trim_line.bounding_box();

  let space_end = last_space_line.bounds.max_x();
  let trim_end = last_trim_line.bounds.max_x();

  assert!(
    space_bbox.max_x() <= space_end + 0.05,
    "expected space-all to keep punctuation within the line bounds (bbox_end={:.3}, line_end={:.3})",
    space_bbox.max_x(),
    space_end
  );
  assert!(
    trim_bbox.max_x() > trim_end + 0.05,
    "expected trim-both to hang closing punctuation past the line end (bbox_end={:.3}, line_end={:.3})",
    trim_bbox.max_x(),
    trim_end
  );
}

#[test]
fn text_spacing_trim_space_first_trims_after_soft_wrap() {
  // Use `<wbr>` to force a soft wrap before the opening punctuation. `space-first` should trim the
  // opening punctuation on the second line (since it's not the first formatted line and not after a
  // forced break).
  let space_all = layout_lines_with_box_style(
    "width: 120px; white-space: normal; word-break: normal; text-align: left; text-spacing-trim: space-all;",
    "HELLO<wbr>「WORLD」",
  );
  let space_first = layout_lines_with_box_style(
    "width: 120px; white-space: normal; word-break: normal; text-align: left; text-spacing-trim: space-first;",
    "HELLO<wbr>「WORLD」",
  );

  assert!(space_all.len() >= 2, "expected soft wrap to produce at least 2 lines");
  assert!(
    space_first.len() >= 2,
    "expected soft wrap to produce at least 2 lines"
  );

  let all_x = first_text_x_in_line(&space_all[1]).expect("first text x (space-all)");
  let first_x = first_text_x_in_line(&space_first[1]).expect("first text x (space-first)");

  assert!(
    all_x >= -0.01,
    "expected space-all to keep line-start punctuation in-flow (x={all_x:.3})"
  );
  assert!(
    first_x < -0.1,
    "expected space-first to hang line-start punctuation after soft wrap (x={first_x:.3})"
  );
}

#[test]
fn text_spacing_trim_space_first_does_not_trim_after_forced_break() {
  // `<br>` creates a forced break; `space-first` should *not* trim the opening punctuation at the
  // start of the following line.
  let space_first = layout_lines_with_box_style(
    "width: 200px; white-space: normal; word-break: normal; text-align: left; text-spacing-trim: space-first;",
    "HELLO<br>「WORLD」",
  );
  let trim_start = layout_lines_with_box_style(
    "width: 200px; white-space: normal; word-break: normal; text-align: left; text-spacing-trim: trim-start;",
    "HELLO<br>「WORLD」",
  );

  assert!(
    space_first.len() >= 2,
    "expected <br> to produce at least 2 lines"
  );
  assert!(trim_start.len() >= 2, "expected <br> to produce at least 2 lines");

  let space_first_x = first_text_x_in_line(&space_first[1]).expect("first text x (space-first)");
  let trim_start_x = first_text_x_in_line(&trim_start[1]).expect("first text x (trim-start)");

  assert!(
    space_first_x >= -0.01,
    "expected space-first not to hang punctuation after forced break (x={space_first_x:.3})"
  );
  assert!(
    trim_start_x < -0.1,
    "expected trim-start to hang punctuation even after forced break (x={trim_start_x:.3})"
  );
}

#[test]
fn text_spacing_trim_normal_trims_line_end_punctuation_only_on_overflow() {
  // Measure the intrinsic width of the text with no trimming in a wide box.
  let wide = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "HELLOWORLD」",
  );
  assert!(!wide.is_empty(), "expected at least one line");
  let content_width = content_max_x_in_line(&wide[0]);

  // Narrow the box by a small amount so the line overflows prior to justification.
  let narrow_width = (content_width - 0.2).max(1.0);
  let space_all = layout_lines_with_box_style(
    &format!(
      "width: {narrow_width:.3}px; white-space: nowrap; text-align: right; text-spacing-trim: space-all;"
    ),
    "HELLOWORLD」",
  );
  let normal = layout_lines_with_box_style(
    &format!(
      "width: {narrow_width:.3}px; white-space: nowrap; text-align: right; text-spacing-trim: normal;"
    ),
    "HELLOWORLD」",
  );

  let line_end = space_all[0].bounds.width();
  let space_over = content_max_x_in_line(&space_all[0]) - line_end;
  let normal_over = content_max_x_in_line(&normal[0]) - line_end;

  assert!(
    space_over > 0.0 && space_over < 0.5,
    "expected space-all to overflow only slightly (overflow={space_over:.3}px)"
  );
  assert!(
    normal_over > 1.0,
    "expected normal to trim the closing punctuation and hang it past the line end (overflow={normal_over:.3}px)"
  );
}

#[test]
fn text_spacing_trim_normal_collapses_adjacent_punctuation_opening_after_closing() {
  // Adjacent-pairs collapsing: in `normal`, an opening punctuation that follows a closing
  // punctuation should be trimmed to half-width.
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span>」</span><span>「</span><span>H</span>",
  );
  let normal = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: normal;",
    "<span>」</span><span>「</span><span>H</span>",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let normal_open_x = text_x_in_line(&normal[0], "「").expect("x for opening punct (normal)");

  assert!(
    normal_open_x < space_open_x - 0.1,
    "expected normal to collapse space between adjacent punctuation (space-all x={space_open_x:.3} normal x={normal_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_normal_collapses_adjacent_punctuation_closing_before_closing() {
  // Adjacent-pairs collapsing: in `normal`, a closing punctuation that precedes another closing
  // punctuation should be trimmed to half-width.
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span>』</span><span>」</span><span>H</span>",
  );
  let normal = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: normal;",
    "<span>』</span><span>」</span><span>H</span>",
  );

  let space_second_x =
    text_x_in_line(&space_all[0], "」").expect("x for second closing (space-all)");
  let normal_second_x = text_x_in_line(&normal[0], "」").expect("x for second closing (normal)");

  assert!(
    normal_second_x < space_second_x - 0.1,
    "expected normal to collapse space for closing+closing pairs (space-all x={space_second_x:.3} normal x={normal_second_x:.3})"
  );
}

#[test]
fn text_spacing_trim_trim_all_trims_punctuation_inside_line() {
  // `trim-all` should trim fullwidth punctuation even when it isn't at a line edge.
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span>H</span><span>「</span><span>H</span>",
  );
  let trim_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-all;",
    "<span>H</span><span>「</span><span>H</span>",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let trim_all_open_x =
    text_x_in_line(&trim_all[0], "「").expect("x for opening punct (trim-all)");

  assert!(
    trim_all_open_x < space_open_x - 0.1,
    "expected trim-all to trim opening punctuation inside the line (space-all x={space_open_x:.3} trim-all x={trim_all_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_normal_collapses_adjacent_punctuation_within_text_run() {
  // Same as the adjacent-pairs tests above, but with no inline-element boundary: the punctuation
  // characters live in the same text node and must still be collapsed.
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "」「H",
  );
  let normal = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: normal;",
    "」「H",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let normal_open_x = text_x_in_line(&normal[0], "「").expect("x for opening punct (normal)");

  assert!(
    normal_open_x < space_open_x - 0.1,
    "expected normal to collapse adjacent punctuation within a single text run (space-all x={space_open_x:.3} normal x={normal_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_trim_all_trims_mid_line_punctuation_within_text_run() {
  // `trim-all` should trim punctuation even when it is not at a line edge and lives inside a
  // single text run (no spans).
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "H「H",
  );
  let trim_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-all;",
    "H「H",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let trim_all_open_x = text_x_in_line(&trim_all[0], "「").expect("x for opening punct (trim-all)");

  assert!(
    trim_all_open_x < space_open_x - 0.1,
    "expected trim-all to trim punctuation inside a single text run (space-all x={space_open_x:.3} trim-all x={trim_all_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_trim_all_trims_middle_dot_punctuation() {
  // `trim-all` should also trim fullwidth middle-dot punctuation.
  // Compare against `trim-both`, which does not trim punctuation in the middle of the line.
  let trim_both = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-both;",
    "A・B",
  );
  let trim_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-all;",
    "A・B",
  );

  let both_b_x = text_x_in_line(&trim_both[0], "B").expect("x for B (trim-both)");
  let all_b_x = text_x_in_line(&trim_all[0], "B").expect("x for B (trim-all)");

  assert!(
    all_b_x < both_b_x - 0.1,
    "expected trim-all to trim middle-dot punctuation (trim-both x={both_b_x:.3} trim-all x={all_b_x:.3})"
  );
}

#[test]
fn text_spacing_trim_normal_collapses_adjacent_punctuation_within_span() {
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span>」「H</span>",
  );
  let normal = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: normal;",
    "<span>」「H</span>",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let normal_open_x = text_x_in_line(&normal[0], "「").expect("x for opening punct (normal)");

  assert!(
    normal_open_x < space_open_x - 0.1,
    "expected normal to collapse adjacent punctuation within a span (space-all x={space_open_x:.3} normal x={normal_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_trim_all_trims_mid_line_punctuation_within_span() {
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span>H「H</span>",
  );
  let trim_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-all;",
    "<span>H「H</span>",
  );

  let space_open_x = text_x_in_line(&space_all[0], "「").expect("x for opening punct (space-all)");
  let trim_all_open_x =
    text_x_in_line(&trim_all[0], "「").expect("x for opening punct (trim-all)");

  assert!(
    trim_all_open_x < space_open_x - 0.1,
    "expected trim-all to trim punctuation within a span (space-all x={space_open_x:.3} trim-all x={trim_all_open_x:.3})"
  );
}

#[test]
fn text_spacing_trim_does_not_shift_inline_box_fragment() {
  // Trimming/hanging should affect the text fragment positioning but should not shift the
  // containing inline box itself (e.g. its background/border positioning).
  let space_all = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: space-all;",
    "<span style=\"background: yellow\">「H</span>",
  );
  let trim_start = layout_lines_with_box_style(
    "width: 300px; white-space: nowrap; text-align: left; text-spacing-trim: trim-start;",
    "<span style=\"background: yellow\">「H</span>",
  );

  let space_span_x =
    inline_x_in_line_containing_text(&space_all[0], "「").expect("span x (space-all)");
  let trim_span_x =
    inline_x_in_line_containing_text(&trim_start[0], "「").expect("span x (trim-start)");

  assert!(
    (trim_span_x - space_span_x).abs() < 0.01,
    "expected inline box fragment to remain anchored while trimming (space-all x={space_span_x:.3} trim-start x={trim_span_x:.3})"
  );
  assert!(
    trim_span_x >= -0.01,
    "expected inline box fragment not to hang into the start edge, got x={trim_span_x:.3}"
  );

  let space_punct_x = text_x_in_line(&space_all[0], "「").expect("punct x (space-all)");
  let trim_punct_x = text_x_in_line(&trim_start[0], "「").expect("punct x (trim-start)");
  assert!(
    trim_punct_x < space_punct_x - 0.1,
    "expected trim-start to shift the punctuation itself (space-all x={space_punct_x:.3} trim-start x={trim_punct_x:.3})"
  );
  assert!(
    trim_punct_x < trim_span_x - 0.1,
    "expected trimmed punctuation to hang outside the inline box fragment (span x={trim_span_x:.3} punct x={trim_punct_x:.3})"
  );
}
