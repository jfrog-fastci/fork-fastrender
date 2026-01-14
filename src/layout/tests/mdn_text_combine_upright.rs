use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig, Point, Rect};

fn find_text_abs_bounds(root: &FragmentNode, needle: &str) -> Option<Rect> {
  fn walk(node: &FragmentNode, needle: &str, abs_origin: Point) -> Option<Rect> {
    let current_origin = abs_origin.translate(Point::new(node.bounds.x(), node.bounds.y()));
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return Some(Rect::from_xywh(
          current_origin.x,
          current_origin.y,
          node.bounds.width(),
          node.bounds.height(),
        ));
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, needle, current_origin) {
        return Some(found);
      }
    }
    None
  }

  walk(root, needle, Point::ZERO)
}

#[test]
fn mdn_text_combine_upright_root_vertical_rl_fills_viewport_and_right_aligns_column() {
  // Mirrors the MDN `text-combine-upright` live sample HTML (loaded directly, not via iframe).
  //
  // Regression coverage: when the root element is `writing-mode: vertical-rl`, the root/initial
  // containing block must still fill the viewport so the first vertical line is anchored to the
  // right edge (block-start for vertical-rl).
  let html = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
html {
  writing-mode: vertical-rl;
  font: 24px serif;
}
.num {
  text-combine-upright: all;
}
</style>
</head>
<body>
<p lang="zh-Hant">
  民國<span class="num">105</span>年<span class="num">4</span>月<span
    class="num"
    >29</span
  >日
</p>
</body>
</html>"#;

  let viewport_width = 320;
  let viewport_height = 240;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, viewport_width, viewport_height)
    .expect("layout document");

  // Root fragment should fill the viewport in the physical X axis even though the root's block axis
  // is horizontal (vertical-rl).
  assert!(
    (fragments.root.bounds.width() - viewport_width as f32).abs() < 0.5,
    "expected root fragment width to match viewport width; got {:.3} (viewport_width={viewport_width}) bounds={:?}",
    fragments.root.bounds.width(),
    fragments.root.bounds
  );

  // The first text column should be on the right for `writing-mode: vertical-rl`. Use the right
  // edge of the first text fragment as a proxy for the column position.
  let text_bounds = find_text_abs_bounds(&fragments.root, "民國")
    .or_else(|| find_text_abs_bounds(&fragments.root, "105"))
    .expect("expected to find a text fragment from the MDN sample");
  let right_edge = text_bounds.x() + text_bounds.width();
  assert!(
    right_edge > viewport_width as f32 - 40.0,
    "expected primary text to be positioned near the right edge; right_edge={right_edge:.3} viewport_width={viewport_width} text_bounds={text_bounds:?}"
  );
}

