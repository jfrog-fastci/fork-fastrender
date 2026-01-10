use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_text_baseline_abs(node: &FragmentNode, needle: &str, parent_y: f32) -> Option<f32> {
  let abs_y = parent_y + node.bounds.y();
  if let FragmentContent::Text {
    text,
    baseline_offset,
    ..
  } = &node.content
  {
    if text.contains(needle) {
      return Some(abs_y + *baseline_offset);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_text_baseline_abs(child, needle, abs_y) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_box_baseline_includes_strut_even_with_nested_font() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let html = r#"
    <html>
      <head>
        <style>
          body {
            margin: 0;
            font-family: sans-serif;
            font-size: 16px;
            line-height: 28px;
          }

          code {
            font-family: monospace;
            padding: 2px 4px;
            background: #eee;
          }
        </style>
      </head>
      <body>
        <p>The <strong><code>text-combine-upright</code></strong> CSS</p>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let plain_baseline =
    find_text_baseline_abs(&tree.root, "The", 0.0).expect("baseline for plain text");
  let code_baseline = find_text_baseline_abs(&tree.root, "text-combine-upright", 0.0)
    .expect("baseline for code text");

  assert!(
    (plain_baseline - code_baseline).abs() < 0.5,
    "expected baselines to align (plain={plain_baseline}, code={code_baseline})"
  );
}
