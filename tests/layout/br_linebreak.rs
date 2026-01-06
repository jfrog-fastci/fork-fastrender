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

fn line_texts(html: &str) -> Vec<String> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
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

fn line_texts_and_heights(html: &str) -> Vec<(String, f32)> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
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
      (text, line.bounds.height())
    })
    .collect()
}

#[test]
fn br_forces_line_break() {
  let lines = line_texts("<p>hello<br>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_self_closing_forces_line_break() {
  let lines = line_texts("<p>hello<br/>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_forces_line_break_under_nowrap() {
  let lines = line_texts("<p style=\"white-space: nowrap\">hello<br>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_preserves_line_height_for_empty_lines() {
  let html = "<p style=\"font-family: 'DejaVu Sans', sans-serif; font-size: 26px; line-height: 1.2\">hello<br>world<br><br>after blank<br>end</p>";

  let lines = line_texts_and_heights(html);
  let texts: Vec<&str> = lines.iter().map(|(t, _)| t.trim()).collect();
  assert_eq!(texts, ["hello", "world", "", "after blank", "end"]);

  let expected = 26.0 * 1.2;
  for (text, height) in lines {
    assert!(
      (height - expected).abs() < 0.05,
      "line {text:?} height={height:.3} expected={expected:.3}"
    );
  }
}
