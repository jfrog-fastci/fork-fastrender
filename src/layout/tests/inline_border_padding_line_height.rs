use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  node.children.iter().find_map(find_first_line)
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

fn find_first_inline_with_text<'a>(
  node: &'a FragmentNode,
  needle: &str,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Inline { .. }) && fragment_contains_text(node, needle)
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_first_inline_with_text(child, needle))
}

#[test]
fn inline_border_and_padding_expand_line_box_height() {
  // Browser engines include an inline box's vertical padding/borders when they extend beyond the
  // line-height strut, so pill-style and bordered inline elements don't get clipped by
  // `overflow: hidden` containers.
  let html = r#"
    <div style="font-family: 'DejaVu Sans', sans-serif; font-size: 11px; line-height: 11px">
      before
      <span style="border: 6px solid black; padding: 5px 4px;">?</span>
      after
    </div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let line = find_first_line(&fragments.root).expect("expected line fragment");
  let inline = find_first_inline_with_text(line, "?").expect("expected inline fragment for '?'");

  assert!(
    line.bounds.height() + 0.1 >= inline.bounds.height(),
    "line box should enclose inline border box: line_height={} inline_height={}",
    line.bounds.height(),
    inline.bounds.height()
  );
  assert!(
    inline.bounds.y() >= -0.01 && inline.bounds.max_y() <= line.bounds.height() + 0.1,
    "inline border box should fit within the line box: y={} max_y={} line_height={}",
    inline.bounds.y(),
    inline.bounds.max_y(),
    line.bounds.height()
  );
}
