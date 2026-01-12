use crate::geometry::{Point, Rect};
use crate::tree::fragment_tree::FragmentContent;
use crate::{FastRender, FragmentNode};

fn find_text_bounds(fragment: &FragmentNode, origin: Point, needle: &str) -> Option<Rect> {
  let absolute_origin = Point::new(
    origin.x + fragment.bounds.x(),
    origin.y + fragment.bounds.y(),
  );

  if let FragmentContent::Text { text, .. } = &fragment.content {
    if text.contains(needle) {
      return Some(Rect::from_xywh(
        absolute_origin.x,
        absolute_origin.y,
        fragment.bounds.width(),
        fragment.bounds.height(),
      ));
    }
  }

  for child in fragment.children.iter() {
    if let Some(found) = find_text_bounds(child, absolute_origin, needle) {
      return Some(found);
    }
  }

  None
}

#[test]
fn external_float_context_width_tracks_containing_block_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; min-width: 900px; font: 16px sans-serif; }
          #left, #right { float: left; width: 410px; height: 30px; }
        </style>
      </head>
      <body>
        <div id="left">left float</div>
        <div id="right">right float</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");

  let tree = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let left = find_text_bounds(&tree.root, Point::new(0.0, 0.0), "left float")
    .expect("left float text fragment");
  let right = find_text_bounds(&tree.root, Point::new(0.0, 0.0), "right float")
    .expect("right float text fragment");

  assert!(
    (left.y() - right.y()).abs() < 1.0,
    "expected floats to share a row (left={left:?}, right={right:?})"
  );
  assert!(
    right.x() > left.x() + 100.0,
    "expected right float to be positioned to the right (left={left:?}, right={right:?})"
  );
}
