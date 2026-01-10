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
fn inline_boxes_can_fragment_without_starting_on_new_line() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 11ch; font: 16px/1 monospace; }
          span { background: yellow; }
        </style>
        <div id="box">Hello <span>world world</span></div>
      "#,
    )
    .expect("parse HTML");

  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();

  assert_eq!(trimmed, ["Hello world", "world"]);
}
