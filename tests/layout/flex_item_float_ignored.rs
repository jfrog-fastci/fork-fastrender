use fastrender::geometry::Point;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

const EPS: f32 = 0.01;

fn layout_html(renderer: &mut FastRender, html: &str) -> fastrender::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer.layout_document(&dom, 400, 400).expect("layout")
}

fn find_block_y_for_text(root: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(
    node: &FragmentNode,
    offset: Point,
    current_block_y: Option<f32>,
    needle: &str,
  ) -> Option<f32> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    let current_block_y = if matches!(node.content, FragmentContent::Block { .. }) {
      Some(abs.y)
    } else {
      current_block_y
    };

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return current_block_y;
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, current_block_y, needle) {
        return Some(found);
      }
    }
    None
  }

  walk(root, Point::ZERO, None, needle)
}

#[test]
fn float_is_ignored_on_flex_items() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // CSS Flexbox: `float` does not apply to flex items. The floated label must still participate in
  // the flex container's main-axis layout, pushing the following item down.
  //
  // Regression: ndtv.com uses `float:left` on an item in a column flex container for an
  // "Advertisement" label. FastRender treated it as a real float, causing it to overlap the ad box
  // below it.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      body { font-family: sans-serif; font-size: 14px; }
      #outer { width: 336px; }
      #container {
        display: flex;
        flex-direction: column;
        align-items: center;
        text-align: center;
        float: left;
        width: 100%;
        padding-bottom: 11px;
      }
      #label {
        display: inline-block;
        float: left;
        width: 100%;
        font-size: 10px;
        line-height: 16px;
      }
      #box { width: 300px; height: 250px; }
    </style>
    <div id="outer"><div id="container"><span id="label">A</span><div id="box">B</div></div></div>
  "#;

  let tree = layout_html(&mut renderer, html);

  let label_y = find_block_y_for_text(&tree.root, "A").expect("find label block");
  let box_y = find_block_y_for_text(&tree.root, "B").expect("find box block");

  assert!(
    (box_y - (label_y + 16.0)).abs() <= EPS,
    "expected flex item B to be stacked below A (A y={label_y}, B y={box_y})"
  );
}
