use crate::style::color::Rgba;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

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

fn find_first_inline_with_bg<'a>(node: &'a FragmentNode, bg: Rgba) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Inline { .. })
    && node
      .style
      .as_deref()
      .is_some_and(|style| style.background_color == bg)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_inline_with_bg(child, bg) {
      return Some(found);
    }
  }
  None
}

fn find_first_text_child<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  for child in node.children.iter() {
    if matches!(child.content, FragmentContent::Text { .. }) {
      return Some(child);
    }
    if let Some(found) = find_first_text_child(child) {
      return Some(found);
    }
  }
  None
}

fn assert_line_padding_geometry(inline: &FragmentNode, expected: f32) {
  let text = find_first_text_child(inline).expect("inline fragment should contain a text child");
  let left = text.bounds.x();
  let right = inline.bounds.width() - (text.bounds.x() + text.bounds.width());
  assert!(
    (left - expected).abs() < 0.6,
    "expected left line-padding ~{expected}px, got {left:.3}px"
  );
  assert!(
    (right - expected).abs() < 0.6,
    "expected right line-padding ~{expected}px, got {right:.3}px"
  );
}

#[test]
fn line_padding_applies_per_line_fragment() {
  let html = r#"
    <style>
      body { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 16px; }
      p { margin: 0; width: 55px; }
      span { background: red; line-padding: 10px; }
    </style>
    <p><span>hello hello</span></p>
  "#;

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
  let lines: Vec<&FragmentNode> = block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .collect();

  assert_eq!(
    lines.len(),
    2,
    "expected 2 line fragments, got {}",
    lines.len()
  );
  for line in lines {
    let inline = find_first_inline_with_bg(line, Rgba::RED).expect("expected span inline fragment");
    assert_line_padding_geometry(inline, 10.0);
  }
}

#[test]
fn line_padding_uses_innermost_inline_box_at_each_edge() {
  // Outer span has no line-padding; inner em has line-padding. Ensure that when the second line
  // starts inside the inner element, only the inner element inserts start padding.
  let html = r#"
    <style>
      body { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 16px; }
      p { margin: 0; width: 55px; }
      span { background: black; line-padding: 0px; color: white; }
      em { background: green; line-padding: 10px; font-style: normal; color: white; }
    </style>
    <p><span>aa <em>bb bb</em></span></p>
  "#;

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
  let lines: Vec<&FragmentNode> = block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .collect();

  assert_eq!(
    lines.len(),
    2,
    "expected 2 line fragments, got {}",
    lines.len()
  );

  // CSS keyword `green` is rgb(0, 128, 0) (not full-bright `lime`).
  let css_green = Rgba::rgb(0, 128, 0);
  let first_em =
    find_first_inline_with_bg(lines[0], css_green).expect("expected em inline fragment");
  let second_em =
    find_first_inline_with_bg(lines[1], css_green).expect("expected em inline fragment");

  let first_text = find_first_text_child(first_em).expect("expected text in first-line em");
  let second_text = find_first_text_child(second_em).expect("expected text in second-line em");

  assert!(
    first_text.bounds.x().abs() < 0.6,
    "expected first-line em to have ~0 start padding, got {:.3}",
    first_text.bounds.x()
  );
  assert!(
    (second_text.bounds.x() - 10.0).abs() < 0.6,
    "expected second-line em to have ~10px start padding, got {:.3}",
    second_text.bounds.x()
  );
}
