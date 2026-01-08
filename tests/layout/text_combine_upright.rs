use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

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

fn find_first_text_fragment<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return Some(node);
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn render_lines(html: &str) -> Vec<String> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 400, 400)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| {
      let mut text = String::new();
      collect_text(line, &mut text);
      text
    })
    .collect()
}

#[test]
fn text_combine_upright_is_measured_as_1em_square_with_centered_baseline() {
  // CSS Writing Modes 4: the combined composition is measured as 1em square, with its own
  // baseline positioned so the square is centered between the parent baselines.
  //
  // Use a parent line-height of 2em to ensure the combined fragment does *not* inherit a 2em
  // block advance: it must still measure as 1em.
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-rl; font-family: 'DejaVu Sans', sans-serif; font-size: 20px; line-height: 2; width: 200px; height: 200px">
          A<span style="text-combine-upright: digits 2">12</span>B
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 400, 400)
    .expect("layout document");

  let frag = find_first_text_fragment(&fragments.root, "12").expect("combined text fragment");
  let (baseline_offset, bounds) = match &frag.content {
    FragmentContent::Text { baseline_offset, .. } => (*baseline_offset, frag.bounds),
    other => panic!("expected FragmentContent::Text, got {other:?}"),
  };

  // In vertical writing mode, `Text` fragment bounds are (block_size, inline_advance).
  let expected = 20.0;
  assert!(
    (bounds.width() - expected).abs() < 0.2,
    "expected combined fragment to be 1em wide: got {:.3} (bounds={bounds:?})",
    bounds.width()
  );
  assert!(
    (bounds.height() - expected).abs() < 0.2,
    "expected combined fragment to advance 1em: got {:.3} (bounds={bounds:?})",
    bounds.height()
  );

  // Baseline should be centered within the 1em square.
  assert!(
    (baseline_offset - bounds.width() * 0.5).abs() < 0.6,
    "expected baseline centered: baseline_offset={baseline_offset:.3} width={:.3}",
    bounds.width()
  );
}

#[test]
fn text_combine_upright_counts_as_a_single_unit_for_line_breaking() {
  // Constrain the inline size (physical height in vertical writing mode) to 2em.
  // With tate-chu-yoko, "A12" should fit in the first line box and "B" should wrap.
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode: vertical-rl; font-family: 'DejaVu Sans', sans-serif; font-size: 20px; line-height: 2; width: 200px; height: 40px">
          A<span style="text-combine-upright: digits 2">12</span>B
        </div>
      </body>
    </html>
  "#;

  let mut lines = render_lines(html);
  for line in &mut lines {
    *line = line.trim().to_string();
  }
  lines.retain(|t| !t.is_empty());
  lines.sort();

  assert_eq!(lines, ["A12", "B"]);
}

