use fastrender::geometry::{Point, Rect};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

const EPS: f32 = 0.01;

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
    let current_block = if matches!(node.content, FragmentContent::Block { .. }) {
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
fn grid_auto_placement_does_not_skip_rows_before_row_locked_items() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // CSS Grid 2 §8.5:
  // Row-locked items are processed before the final auto-placement step, but they must not advance
  // the auto-placement cursor. Otherwise fully-auto items can be placed into *implicit* rows after
  // the row-locked items, leaving earlier explicit rows empty.
  //
  // Real-world impact: "sticky footer" layouts with:
  //   grid-template-rows: 1fr auto;
  //   footer { grid-row-start: 2; grid-row-end: 3; }
  // rely on auto-placed content remaining in the first row.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #grid {
        display: grid;
        grid-template-columns: 100px;
        grid-template-rows: 10px 10px;
      }
      #b {
        grid-row-start: 2;
        grid-row-end: 3;
      }
    </style>
    <div id="grid">
      <div id="a">A</div>
      <div id="b">B</div>
    </div>
  "#;

  let tree = layout_html(&mut renderer, html, 200, 100);
  let a_bounds = find_block_bounds_for_text(&tree.root, "A").expect("find A");
  let b_bounds = find_block_bounds_for_text(&tree.root, "B").expect("find B");

  assert!(
    (a_bounds.y() - 0.0).abs() <= EPS,
    "expected auto-placed #a to remain in row 1 at y=0, got {}",
    a_bounds.y()
  );
  assert!(
    (b_bounds.y() - 10.0).abs() <= EPS,
    "expected row-locked #b to be placed in row 2 at y=10, got {}",
    b_bounds.y()
  );
}
