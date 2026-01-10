use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_line(child) {
      return Some(found);
    }
  }
  None
}

fn fragment_contains_text(node: &FragmentNode, needle: &str) -> bool {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return true;
    }
  }
  node
    .children
    .iter()
    .any(|child| fragment_contains_text(child, needle))
}

fn find_first_inline_with_text<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Inline { .. }) && fragment_contains_text(node, needle) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_inline_with_text(child, needle) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_padding_border_box_uses_content_area_not_line_height() {
  // CSS 2.1 §10.6.1: for inline non-replaced elements, vertical padding/border begins at the
  // content area's edges (font metrics), and is not tied to the line-height strut.
  //
  // Regression test: when line-height introduces large leading, an inline element's border box
  // should sit *inside* the line-height box (offset by half-leading), rather than expanding to the
  // full line-height.
  let html = r#"
    <style>
      p {
        font-family: 'DejaVu Sans', sans-serif;
        font-size: 10px;
        line-height: 100px;
      }
      code {
        background: rgb(255, 0, 0);
        padding: 2px 0;
      }
    </style>
    <p>before <code>code</code> after</p>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let line = find_first_line(&fragments.root).expect("expected a line fragment");
  let line_height = line.bounds.height();
  // Mixing fonts (e.g. UA defaults for `<code>`) can cause tiny ascent/descent mismatches and
  // slightly inflate the line box beyond the authored `line-height`, so don't assert exactness.
  assert!(line_height > 90.0, "unexpected line height: {line_height}");

  let code_inline = find_first_inline_with_text(line, "code").expect("expected <code> inline fragment");

  // Child fragment coordinates are relative to the line fragment. The inline <code> border box
  // should not start above the line box when line-height is much larger than the font's content box.
  assert!(
    code_inline.bounds.y() >= 0.0,
    "expected inline <code> border box to be inside the line-height box, got y={}",
    code_inline.bounds.y()
  );
  assert!(
    code_inline.bounds.height() < line_height,
    "expected inline <code> border box to be smaller than the line-height box ({}), got {}",
    line_height,
    code_inline.bounds.height()
  );
  assert!(
    code_inline.bounds.max_y() <= line_height + 0.1,
    "expected inline <code> border box to fit within the line-height box, got y={} height={} line_height={}",
    code_inline.bounds.y(),
    code_inline.bounds.height(),
    line_height
  );
}
