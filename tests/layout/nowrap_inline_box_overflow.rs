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

fn line_texts(html: &str, width: u32) -> Vec<String> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, width, 200)
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
fn nowrap_does_not_soft_wrap_before_inline_boxes() {
  // Regression test: inline boxes inside a `white-space: nowrap` container should stay on the
  // same line even when they overflow the available width. Previously we would soft-wrap at the
  // boundary between inline boxes (e.g. an icon span followed by text), which inflated line boxes
  // and broke real-world UIs like the Microsoft UHF footer language selector.
  let html = r#"
    <div style="font-family: 'DejaVu Sans', sans-serif; font-size: 16px; white-space: nowrap">
      <span style="display: inline-block; width: 20px; height: 20px; background: #000"></span><span>Hello</span>
    </div>
  "#;
  let lines = line_texts(html, 30);
  assert_eq!(lines, ["Hello"]);
}

#[test]
fn nbsp_does_not_soft_wrap_before_inline_block_pseudo_element() {
  // Regression: `&nbsp;` must suppress soft-wrap opportunities across atomic inline boundaries.
  // This matters for patterns like `Community&nbsp;<span class="caret"></span>` where the caret
  // should stay glued to the preceding word and overflow instead of wrapping onto a new line.
  let html = r#"
    <style>
      .live::after { content: "LongLongLongLong"; display: inline-block }
    </style>
    <div style="width: 20px; font-family: 'DejaVu Sans', sans-serif; font-size: 16px">
      <span class="live">A&nbsp;</span>
    </div>
  "#;
  let lines = line_texts(html, 200);
  assert_eq!(lines, ["A\u{00A0}LongLongLongLong"]);
}
