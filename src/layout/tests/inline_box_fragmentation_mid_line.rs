use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

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
fn inline_box_can_fragment_mid_line_instead_of_forcing_break_before() {
  // Regression test: inline boxes should not be treated as atomic for line breaking.
  //
  // An inline element like `<span>` can be fragmented across lines; when it doesn't fit in the
  // remaining line width, browsers place as much of its content as possible on the current line.
  // Previously we forced a break *before* the inline box whenever it didn't fit entirely, which
  // left lines underfull and increased line count.
  let html = r#"
    <div style="font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 20px">
      Hello <span>world world world world world world</span>
    </div>
  "#;

  let lines = line_texts(html, 180);
  assert!(
    lines.len() >= 2,
    "expected wrapping to produce multiple lines, got: {lines:?}"
  );

  let first = lines[0].trim();
  assert!(
    first.contains("world"),
    "expected first line to include text from the `<span>`, got: {lines:?}"
  );
}
