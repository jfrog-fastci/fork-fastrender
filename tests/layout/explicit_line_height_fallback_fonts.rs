use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

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

fn line_heights(html: &str) -> Vec<f32> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let block =
    find_first_block_with_line_children(&fragments.root).expect("block with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| line.bounds.height())
    .collect()
}

#[test]
fn explicit_line_height_does_not_inflate_for_mixed_script_fallback_fonts() {
  let html = r#"<div style="font-family: sans-serif; font-size: 13px; line-height: 1.4; white-space: nowrap"><span>English</span> <span>ಕ್ಷ</span></div>"#;

  let heights = line_heights(html);
  assert_eq!(heights.len(), 1);

  let expected = 13.0 * 1.4;
  let height = heights[0];
  assert!(
    (height - expected).abs() < 0.2,
    "expected line box height to match authored line-height, got {height:.3} (expected {expected:.3})"
  );
}
