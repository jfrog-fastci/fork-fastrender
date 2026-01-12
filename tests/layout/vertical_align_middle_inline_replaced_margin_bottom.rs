use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn contains_text(fragment: &FragmentNode, needle: &str) -> bool {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return true;
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

fn find_line_with_text_abs_y<'a>(
  node: &'a FragmentNode,
  needle: &str,
  parent_y: f32,
) -> Option<(f32, &'a FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Line { .. }) && contains_text(node, needle) {
    return Some((abs_y, node));
  }
  for child in node.children.iter() {
    if let Some(found) = find_line_with_text_abs_y(child, needle, abs_y) {
      return Some(found);
    }
  }
  None
}

fn collect_replaced_abs_y<'a>(
  node: &'a FragmentNode,
  parent_y: f32,
  out: &mut Vec<(f32, &'a FragmentNode)>,
) {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    out.push((abs_y, node));
  }
  for child in node.children.iter() {
    collect_replaced_abs_y(child, abs_y, out);
  }
}

#[test]
fn vertical_align_middle_ignores_margin_bottom_for_inline_replaced_positioning() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let data_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=";

  // Lobste.rs uses `margin-bottom` + `vertical-align: middle` on small avatar images. The CSS
  // `vertical-align` middle algorithm should align the *replaced element* box midpoint; vertical
  // margins must not shift the border box up/down.
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          .avatar {{ vertical-align: middle; }}
          .mb {{ margin-bottom: 2px; }}
        </style>
      </head>
      <body style="margin:0; font-family:sans-serif; font-size:20px; line-height:20px">
        <div>
          <img class="avatar" src="{data_png}" style="width:16px; height:16px">
          <img class="avatar mb" src="{data_png}" style="width:16px; height:16px">
          TXT
        </div>
      </body>
    </html>
  "#,
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let (line_y, line) = find_line_with_text_abs_y(&tree.root, "TXT", 0.0).expect("line");
  let mut replaced = Vec::new();
  collect_replaced_abs_y(line, line_y, &mut replaced);
  assert_eq!(
    replaced.len(),
    2,
    "expected exactly 2 replaced fragments on the test line"
  );

  // The two images have identical intrinsic sizes; `margin-bottom` should not change the border-box
  // y-position under `vertical-align: middle`.
  let img0_y = replaced[0].0;
  let img1_y = replaced[1].0;
  assert!(
    (img0_y - img1_y).abs() < 0.5,
    "expected images to have equal y positions (no-margin={img0_y:.2}, margin-bottom={img1_y:.2})"
  );
}
