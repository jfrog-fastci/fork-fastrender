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

fn collect_line_texts(node: &FragmentNode, out: &mut Vec<String>) {
  if matches!(node.content, FragmentContent::Line { .. }) {
    let mut text = String::new();
    collect_text(node, &mut text);
    out.push(text);
  }
  for child in node.children.iter() {
    collect_line_texts(child, out);
  }
}

#[test]
fn leading_whitespace_after_outside_list_marker_is_ignored() {
  // Pretty-printed HTML commonly inserts indentation whitespace between `<li>` and the first
  // inline-level element. In browsers this whitespace is treated as leading whitespace in the line
  // box and is suppressed, so it should not introduce an extra gap after the list marker.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html("<ul><li>\n  <a href=\"#\">Item</a>\n</li></ul>")
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let item_lines: Vec<String> = lines.into_iter().filter(|line| line.contains("Item")).collect();
  assert_eq!(item_lines.len(), 1, "expected one line containing the list item text");
  assert_eq!(item_lines[0].trim_end(), "• Item");
}

