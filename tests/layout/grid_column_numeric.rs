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
fn grid_column_accepts_numeric_values() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // `grid-column: 1` is a common shorthand that is parsed as a numeric token. The engine must treat
  // it as a grid placement, not ignore it.
  //
  // The second `#a` rule overrides the first. If numeric `grid-column` values are dropped, `#a`
  // incorrectly keeps spanning two columns, and `#b` ends up auto-placed into column 3.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #grid { display: grid; grid-template-columns: 100px 100px 100px 100px; }
      #a { grid-row: 1; grid-column: 1 / span 2; height: 10px; background: red; }
      #a { grid-column: 1; }
      #b { grid-row: 1; grid-column: 2; height: 10px; background: blue; }
    </style>
    <div id="grid">
      <div id="a">A</div>
      <div id="b">B</div>
    </div>
  "#;

  let tree = layout_html(&mut renderer, html, 400, 100);

  let a_bounds = find_block_bounds_for_text(&tree.root, "A").expect("find A");
  let b_bounds = find_block_bounds_for_text(&tree.root, "B").expect("find B");

  assert!(
    (a_bounds.x() - 0.0).abs() <= EPS,
    "expected #a to start in column 1 at x=0, got {}",
    a_bounds.x()
  );
  assert!(
    (a_bounds.width() - 100.0).abs() <= EPS,
    "expected #a to span exactly one 100px track, got width={}",
    a_bounds.width()
  );

  assert!(
    (b_bounds.x() - 100.0).abs() <= EPS,
    "expected #b to be placed in column 2 at x=100, got {}",
    b_bounds.x()
  );
  assert!(
    (b_bounds.width() - 100.0).abs() <= EPS,
    "expected #b to span exactly one 100px track, got width={}",
    b_bounds.width()
  );
}

