use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

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

fn find_replaced_abs_y<'a>(
  node: &'a FragmentNode,
  parent_y: f32,
) -> Option<(f32, &'a FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    return Some((abs_y, node));
  }
  for child in node.children.iter() {
    if let Some(found) = find_replaced_abs_y(child, abs_y) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_box_baseline_uses_last_baseline_relative_child() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let data_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=";

  // The inline box (<a>) starts with a large replaced element and ends with text. Per CSS 2.1,
  // the baseline of an inline box is the baseline of its *last* in-flow line box; in the common
  // single-line case, that means the baseline should come from the trailing text, not the leading
  // replaced element.
  //
  // When the baseline is (incorrectly) taken from the first child, the line baseline can be pushed
  // down to the replaced element's bottom edge, effectively canceling `vertical-align: middle` and
  // inflating the line box height.
  let html = format!(
    r##"
    <html>
      <head>
        <style>
          .v-mid {{ vertical-align: middle; }}
        </style>
      </head>
      <body style="margin:0; font-family:sans-serif; font-size:36px; line-height:57.6px">
        <div>
          <a href="#">
            <img class="v-mid" src="{data_png}" style="width:80px; height:80px; margin-top:-3px">
            TEXT
          </a>
        </div>
      </body>
    </html>
  "##,
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let (line_y, line) =
    find_line_with_text_abs_y(&tree.root, "TEXT", 0.0).expect("TEXT line fragment");
  let FragmentContent::Line { baseline } = line.content else {
    panic!("expected TEXT fragment to be a line");
  };
  let baseline_abs = line_y + baseline;

  let (img_y, img) = line
    .children
    .iter()
    .find_map(|child| find_replaced_abs_y(child, line_y))
    .expect("line should contain a replaced fragment");
  let img_bottom_abs = img_y + img.bounds.height();

  assert!(
    img_bottom_abs > baseline_abs + 0.5,
    "expected vertical-align: middle image to have its bottom edge below the line baseline when inline box baseline is taken from the trailing text (img_bottom={img_bottom_abs}, baseline={baseline_abs})"
  );

  let line_height = line.bounds.height();
  assert!(
    line_height < 85.0,
    "expected line box height to be close to the replaced element size, not inflated by an incorrect inline box baseline (line_height={line_height})"
  );
}
