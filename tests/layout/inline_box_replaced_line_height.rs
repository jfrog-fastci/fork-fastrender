use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn contains_replaced(node: &FragmentNode) -> bool {
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    return true;
  }
  node.children.iter().any(contains_replaced)
}

fn find_first_line_with_replaced<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) && contains_replaced(node) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_line_with_replaced(child) {
      return Some(found);
    }
  }
  None
}

fn find_first_replaced_position(
  node: &FragmentNode,
  origin: (f32, f32),
) -> Option<(f32, f32, f32, f32)> {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    return Some((pos.0, pos.1, node.bounds.width(), node.bounds.height()));
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_replaced_position(child, pos) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_box_only_line_expands_for_replaced_content() {
  // Regression: when a line contains only an inline box (e.g. an <a>/<span>) whose contents are a
  // tall replaced element, the line box baseline/height must include the inline box's subtree.
  //
  // Previously the line baseline accumulator updated ascent/descent for inline boxes but forgot to
  // mark the line as containing any items, so the finalized line box used only the strut metrics.
  // This positioned the inline box at a large negative y to align its (correct) baseline with the
  // (incorrect) line baseline, clipping content like the Cornell logo on arxiv.org.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      p { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 10px; line-height: 1; }
      img { width: 20px; height: 40px; }
    </style>
    <p><a href="test"><img src="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'></svg>"></a></p>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let line = find_first_line_with_replaced(&fragments.root).expect("expected a line with replaced content");
  let FragmentContent::Line { baseline } = line.content else {
    panic!("expected line fragment");
  };

  let (_, image_y, _, image_height) =
    find_first_replaced_position(line, (0.0, 0.0)).expect("expected replaced fragment under line");

  assert!(
    image_y >= -0.01,
    "expected replaced fragment to start at or below the line top, got y={image_y}"
  );
  assert!(
    baseline >= image_height - 0.5,
    "expected line baseline ({baseline}) to be at least the replaced height ({image_height})"
  );
  assert!(
    line.bounds.height() >= image_height - 0.5,
    "expected line height ({}) to be at least the replaced height ({image_height})",
    line.bounds.height()
  );
}
