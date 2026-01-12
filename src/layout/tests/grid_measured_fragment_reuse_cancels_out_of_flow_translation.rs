use crate::geometry::{Point, Rect};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

const EPS: f32 = 0.5;

fn layout_html(
  renderer: &mut FastRender,
  html: &str,
  viewport_width: u32,
  viewport_height: u32,
) -> crate::tree::fragment_tree::FragmentTree {
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
fn grid_measured_fragment_reuse_cancels_out_of_flow_descendant_translation() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // Regression test:
  // Grid items are measured at origin=(0,0) and later translated into their final grid-area
  // position. Out-of-flow positioned descendants whose containing block is outside the grid item
  // subtree must not inherit that translation.
  //
  // With the bug, the absolutely positioned `#overlay` element ends up shifted into the second
  // grid column (x≈100px) instead of staying pinned to the initial containing block at x=0.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #grid {
        display: grid;
        width: 200px;
        grid-template-columns: 100px 100px;
      }
      #item { grid-column: 2; }
      #overlay {
        position: absolute;
        top: 0; right: 0; bottom: 0; left: 0;
        font-size: 1px;
        line-height: 1px;
      }
    </style>
    <div id="grid">
      <div id="item">
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
    (overlay_bounds.width() - 200.0).abs() <= EPS,
    "expected overlay width≈200, got {}",
    overlay_bounds.width()
  );
  assert!(
    (overlay_bounds.height() - 200.0).abs() <= EPS,
    "expected overlay height≈200, got {}",
    overlay_bounds.height()
  );
}
