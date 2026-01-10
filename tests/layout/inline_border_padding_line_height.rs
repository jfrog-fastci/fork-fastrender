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
    .layout_document(&dom, 800, 600)
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
fn inline_border_and_padding_do_not_inflate_line_box_height() {
  let html = r#"
    <div style="font-family: 'DejaVu Sans', sans-serif; font-size: 11px; line-height: 11px">
      before
      <span style="border: 6px solid black; padding: 5px 4px;">?</span>
      after
    </div>
  "#;

  let heights = line_heights(html);
  assert_eq!(heights.len(), 1);

  let expected = 11.0;
  let height = heights[0];
  assert!(
    (height - expected).abs() < 0.1,
    "expected line box height to be governed by line-height, got {height:.3} (expected {expected:.3})"
  );
}

