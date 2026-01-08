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
fn whitespace_only_inline_boxes_do_not_create_empty_lines() {
  // When a block container has mixed inline + block children, the inline segment is wrapped in an
  // anonymous block. Inline boxes that contain only collapsible whitespace should not force that
  // anonymous block to create an empty line box with non-zero height.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer
    .parse_html("<span>\n    </span><h1 style=\"margin:0\">Title</h1>")
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
  assert_eq!(trimmed, ["Title"]);
}

