use fastrender::geometry::{Point, Rect};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

const EPS: f32 = 0.5;

fn layout_html(
  renderer: &mut FastRender,
  html: &str,
  viewport_width: u32,
  viewport_height: u32,
) -> fastrender::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer
    .layout_document(&dom, viewport_width, viewport_height)
    .expect("layout")
}

fn find_block_bounds_for_text(root: &FragmentNode, needle: &str) -> Option<Rect> {
  fn walk(
    node: &FragmentNode,
    offset: Point,
    current_block: Option<Rect>,
    needle: &str,
  ) -> Option<Rect> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    let current_block = if matches!(node.content, FragmentContent::Block { box_id: Some(_) }) {
      Some(Rect::from_xywh(
        abs.x,
        abs.y,
        node.bounds.width(),
        node.bounds.height(),
      ))
    } else {
      current_block
    };

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return current_block;
      }
    }

    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, current_block, needle) {
        return Some(found);
      }
    }

    None
  }

  walk(root, Point::ZERO, None, needle)
}

#[test]
fn flex_measured_fragment_reuse_respects_updated_positioned_containing_block() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // Regression test:
  // A `position:relative` flex container establishes the containing block for absolutely positioned
  // descendants, even when those descendants live inside a flex item. During flex layout, Taffy
  // measures flex items before the container's final `height:auto` is known; fragment reuse must
  // not capture absolute positioning against the initial containing block (viewport).
  //
  // With the bug, the `#overlay` element ends up sized to the viewport height (200px) instead of
  // the flex container's used height (10px).
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #container {
        position: relative;
        display: flex;
        flex-direction: column;
        width: 100px;
      }
      #content { height: 10px; font-size: 1px; line-height: 1px; }
      #overlay {
        position: absolute;
        top: 0; right: 0; bottom: 0; left: 0;
        font-size: 1px;
        line-height: 1px;
      }
    </style>
    <div id="container">
      <div id="item">
        <div id="content">CONTENT</div>
        <div id="overlay">OVERLAY</div>
      </div>
    </div>
  "#;

  let tree = layout_html(&mut renderer, html, 200, 200);
  let overlay_bounds = find_block_bounds_for_text(&tree.root, "OVERLAY").expect("find overlay");

  assert!(
    (overlay_bounds.x() - 0.0).abs() <= EPS,
    "expected overlay x≈0, got {}",
    overlay_bounds.x()
  );
  assert!(
    (overlay_bounds.y() - 0.0).abs() <= EPS,
    "expected overlay y≈0, got {}",
    overlay_bounds.y()
  );
  assert!(
    (overlay_bounds.width() - 100.0).abs() <= EPS,
    "expected overlay width≈100, got {}",
    overlay_bounds.width()
  );
  assert!(
    (overlay_bounds.height() - 10.0).abs() <= EPS,
    "expected overlay height≈10, got {}",
    overlay_bounds.height()
  );
}
