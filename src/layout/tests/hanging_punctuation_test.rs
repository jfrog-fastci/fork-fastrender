use crate::geometry::{Point, Rect};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::text::pipeline::ShapedRun;
use crate::{FastRender, FontConfig};

fn collect_line_fragments<'a>(
  node: &'a FragmentNode,
  origin: Point,
  out: &mut Vec<(f32, &'a FragmentNode)>,
) {
  let abs_bounds = node.bounds.translate(origin);
  if matches!(node.content, FragmentContent::Line { .. }) {
    out.push((abs_bounds.y(), node));
  }
  let child_origin = origin.translate(node.bounds.origin);
  for child in node.children.iter() {
    collect_line_fragments(child, child_origin, out);
  }
}

fn node_contains_text(node: &FragmentNode, needle: &str) -> bool {
  match &node.content {
    FragmentContent::Text { text, .. } => text.contains(needle),
    _ => node.children.iter().any(|child| node_contains_text(child, needle)),
  }
}

fn collect_text_fragments<'a>(
  node: &'a FragmentNode,
  origin: Point,
  out: &mut Vec<(Rect, &'a FragmentNode)>,
) {
  let abs_bounds = node.bounds.translate(origin);
  if matches!(node.content, FragmentContent::Text { .. }) {
    out.push((abs_bounds, node));
  }
  let child_origin = origin.translate(node.bounds.origin);
  for child in node.children.iter() {
    collect_text_fragments(child, child_origin, out);
  }
}

fn find_text_fragment<'a>(line: &'a FragmentNode, needle: &str) -> Option<(Rect, &'a FragmentNode)> {
  let mut text_nodes = Vec::new();
  collect_text_fragments(line, Point::ZERO, &mut text_nodes);
  text_nodes.into_iter().find(|(_, node)| {
    matches!(&node.content, FragmentContent::Text { text, .. } if text.as_ref() == needle)
  })
}

fn find_text_fragment_with_char<'a>(
  line: &'a FragmentNode,
  ch: char,
) -> Option<(Rect, &'a FragmentNode, usize)> {
  let mut text_nodes = Vec::new();
  collect_text_fragments(line, Point::ZERO, &mut text_nodes);
  for (bounds, node) in text_nodes {
    let FragmentContent::Text { text, .. } = &node.content else {
      continue;
    };
    if let Some(byte_offset) = text.find(ch) {
      return Some((bounds, node, byte_offset));
    }
  }
  None
}

fn shaped_runs(fragment: &FragmentNode) -> Option<&[ShapedRun]> {
  match &fragment.content {
    FragmentContent::Text { shaped, .. } => shaped.as_deref().map(|runs| runs.as_slice()),
    _ => None,
  }
}

fn glyph_origin_x_for_cluster(runs: &[ShapedRun], cluster: usize) -> Option<f32> {
  for run in runs {
    let mut x = 0.0f32;
    for glyph in &run.glyphs {
      let glyph_cluster = run.start.saturating_add(glyph.cluster as usize);
      let origin = x + glyph.x_offset;
      if glyph_cluster == cluster {
        return Some(origin);
      }
      x += glyph.x_advance;
    }
  }
  None
}

fn glyph_x_in_line(bounds: Rect, text_fragment: &FragmentNode, cluster: usize) -> Option<f32> {
  let runs = shaped_runs(text_fragment)?;
  let local = glyph_origin_x_for_cluster(runs, cluster)?;
  Some(bounds.x() + local)
}

#[test]
fn hanging_punctuation_first_hangs_start_and_excludes_from_center_measurement() {
  let html = r#"
    <style>
      .container { width: 100px; }
      p {
        margin: 0;
        font-family: "Noto Sans SC";
        font-size: 20px;
        line-height: 20px;
        text-align: center;
        white-space: nowrap;
      }
      #none { hanging-punctuation: none; }
      #first { hanging-punctuation: first; }
    </style>
    <div class="container">
      <p id="none">「Hello」</p>
      <p id="first">「Hello」</p>
    </div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_fragments(&fragments.root, Point::ZERO, &mut lines);
  // Filter out whitespace-only blocks/anonymous wrappers by looking for the target text.
  let mut hello_lines: Vec<(f32, &FragmentNode)> = lines
    .into_iter()
    .filter(|(_, line)| node_contains_text(line, "Hello"))
    .collect();
  hello_lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
  assert!(
    hello_lines.len() >= 2,
    "expected at least two line fragments containing \"Hello\", got {}",
    hello_lines.len()
  );

  let none_line = hello_lines[0].1;
  let first_line = hello_lines[1].1;

  // In the `first` case the opening punctuation should be split into its own text fragment and
  // shifted into negative inline coordinates (hanging past the line start).
  let (open_bounds, _open) =
    find_text_fragment(first_line, "「").expect("expected opening punctuation fragment");
  assert!(
    open_bounds.x() < 0.0,
    "expected opening punctuation to hang past the line start (x<0), got x={:.2}",
    open_bounds.x()
  );

  // Compare the x-position of the 'H' glyph between the two paragraphs.
  let (none_bounds, none_text, none_h_off) =
    find_text_fragment_with_char(none_line, 'H').expect("expected to find 'H' in none case");
  let (first_bounds, first_text, first_h_off) =
    find_text_fragment_with_char(first_line, 'H').expect("expected to find 'H' in first case");

  let h_none =
    glyph_x_in_line(none_bounds, none_text, none_h_off).expect("expected 'H' glyph in none case");
  let h_first =
    glyph_x_in_line(first_bounds, first_text, first_h_off).expect("expected 'H' glyph in first case");

  assert!(
    (h_first - h_none).abs() > 1.0,
    "expected hanging punctuation to change alignment measurement (h_first={:.2} h_none={:.2})",
    h_first,
    h_none
  );
}

#[test]
fn hanging_punctuation_force_end_hangs_punctuation_past_line_end_for_justify() {
  let html = r#"
    <style>
      .container { width: 200px; }
      p {
        margin: 0;
        font-family: "DejaVu Sans";
        font-size: 20px;
        line-height: 20px;
        text-align: justify-all;
        white-space: nowrap;
      }
      #none { hanging-punctuation: none; }
      #force { hanging-punctuation: force-end; }
    </style>
    <div class="container">
      <p id="none"><span>Hello</span> <span>world</span><span>.</span></p>
      <p id="force"><span>Hello</span> <span>world</span><span>.</span></p>
    </div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_fragments(&fragments.root, Point::ZERO, &mut lines);
  let mut hello_lines: Vec<(f32, &FragmentNode)> = lines
    .into_iter()
    .filter(|(_, line)| node_contains_text(line, "Hello"))
    .collect();
  hello_lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
  assert!(hello_lines.len() >= 2, "expected two paragraph lines");

  let none_line = hello_lines[0].1;
  let force_line = hello_lines[1].1;

  let (none_dot_bounds, _none_dot) =
    find_text_fragment(none_line, ".").expect("expected '.' fragment (none)");
  let (force_dot_bounds, _force_dot) =
    find_text_fragment(force_line, ".").expect("expected '.' fragment (force-end)");

  let line_end = force_line.bounds.width();

  assert!(
    (force_dot_bounds.x() - line_end).abs() < 1.0,
    "expected hanging '.' to start at the line end (x≈{:.2}), got x={:.2}",
    line_end,
    force_dot_bounds.x()
  );
  assert!(
    force_dot_bounds.max_x() > line_end + 0.5,
    "expected hanging '.' to extend past the line end: max_x={:.2} line_end={:.2}",
    force_dot_bounds.max_x(),
    line_end
  );

  // In the non-hanging case, the '.' should stay inside the line box.
  assert!(
    none_dot_bounds.max_x() <= none_line.bounds.width() + 0.5,
    "expected non-hanging '.' to stay within the line box: max_x={:.2} line_end={:.2}",
    none_dot_bounds.max_x(),
    none_line.bounds.width()
  );
  assert!(
    force_dot_bounds.x() > none_dot_bounds.x() + 0.5,
    "expected hanging '.' to be positioned further right than non-hanging case (force_x={:.2} none_x={:.2})",
    force_dot_bounds.x(),
    none_dot_bounds.x()
  );
}
