use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FontConfig};

fn find_text_fragment_abs_y<'a>(
  node: &'a fastrender::FragmentNode,
  needle: &str,
  parent_y: f32,
) -> Option<(f32, &'a fastrender::FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return Some((abs_y, node));
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_text_fragment_abs_y(child, needle, abs_y) {
      return Some(found);
    }
  }
  None
}

#[test]
fn vertical_align_super_and_sub_move_in_expected_directions() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let html = r#"
    <html>
      <body style="margin:0; font-family:sans-serif; font-size:20px; line-height:20px">
        <span>AAA</span><span style="vertical-align:super">BBB</span><span style="vertical-align:sub">CCC</span>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let (a_y, _) = find_text_fragment_abs_y(&tree.root, "AAA", 0.0).expect("AAA fragment");
  let (b_y, _) = find_text_fragment_abs_y(&tree.root, "BBB", 0.0).expect("BBB fragment");
  let (c_y, _) = find_text_fragment_abs_y(&tree.root, "CCC", 0.0).expect("CCC fragment");

  assert!(
    b_y < a_y,
    "expected `vertical-align: super` fragment to be above baseline text: super_y={b_y} baseline_y={a_y}",
  );
  assert!(
    c_y > a_y,
    "expected `vertical-align: sub` fragment to be below baseline text: sub_y={c_y} baseline_y={a_y}",
  );
}
