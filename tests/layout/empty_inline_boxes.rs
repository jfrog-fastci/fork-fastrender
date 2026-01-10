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

#[test]
fn abspos_only_inline_boxes_do_not_create_empty_lines() {
  // Inline boxes that only contain out-of-flow positioned descendants should not create
  // placeholder line boxes. These descendants are represented internally as static-position
  // anchors, which should not contribute line box height.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer
    .parse_html(
      "<span><span style=\"position:absolute;top:0;left:0\"></span></span><h1 style=\"margin:0\">Title</h1>",
    )
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
  assert_eq!(trimmed, ["Title"]);
}

#[test]
fn abspos_inline_boxes_do_not_create_empty_lines_before_text() {
  // Regresses a case where a leading static-position anchor (from an absolutely positioned
  // descendant / pseudo-element) prevented a nested inline box from fragmenting onto the same
  // line, producing a zero-width inline box fragment and an empty line box before the text.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 80px; font: 16px/1 monospace; }
        </style>
        <div id="box">
          <a href="test">
            <span style="position:absolute;top:0;left:0"></span>
            <span>world world</span>
          </a>
        </div>"#,
    )
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
  assert_eq!(trimmed, ["world", "world"]);
}
