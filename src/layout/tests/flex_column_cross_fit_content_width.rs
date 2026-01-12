use crate::api::FastRender;
use crate::geometry::{Point, Rect};
use crate::tree::fragment_tree::FragmentNode;
use crate::{FontConfig, Rgba};

fn build_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer")
}

fn find_fragment_by_background(node: &FragmentNode, offset: Point, color: Rgba) -> Option<Rect> {
  let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(Rect::new(abs, node.bounds.size));
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, abs, color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn flex_column_cross_size_auto_is_fit_content_width() {
  // Regression test for flex column containers with `align-items:flex-start`:
  //
  // When a flex item's cross size is `auto` and it is not stretched, the used cross size is a
  // content-based fit-content (shrink-to-fit) size. This must:
  // - shrink small items to their max-content width
  // - clamp large, wrappable content to the available cross size so text wraps (e.g. nasa.gov card overlays)
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; }
      #container {
        display: flex;
        flex-direction: column;
        align-items: flex-start;
        width: 200px;
        font: 16px/20px serif;
      }
      #short { background: rgb(255, 0, 0); }
      #long { background: rgb(0, 0, 255); }
    </style>
    <div id="container">
      <div id="short">Article</div>
      <div id="long">Artemis II Flight Crew, Teams Conduct Demonstration Ahead of Launch</div>
    </div>
  "#;

  let mut renderer = build_renderer();
  let dom = renderer.parse_html(html).expect("parse HTML");
  let tree = renderer.layout_document(&dom, 400, 300).expect("layout");

  let red = Rgba::rgb(255, 0, 0);
  let blue = Rgba::rgb(0, 0, 255);
  let short_bounds =
    find_fragment_by_background(&tree.root, Point::ZERO, red).expect("find short item");
  let long_bounds =
    find_fragment_by_background(&tree.root, Point::ZERO, blue).expect("find long item");

  assert!(
    short_bounds.width() < 150.0,
    "expected short flex item to shrink below the 200px container width; got {}",
    short_bounds.width()
  );
  assert!(
    (long_bounds.width() - 200.0).abs() < 1.0,
    "expected long flex item to clamp to the 200px available width; got {}",
    long_bounds.width()
  );
  assert!(
    long_bounds.height() > 25.0,
    "expected long flex item to wrap onto multiple lines (height > 1 line); got {}",
    long_bounds.height()
  );
}
