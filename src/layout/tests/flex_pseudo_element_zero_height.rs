use crate::geometry::Point;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

const EPS: f32 = 0.01;

fn build_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer")
}

fn layout_html(
  renderer: &mut FastRender,
  html: &str,
) -> crate::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer.layout_document(&dom, 200, 200).expect("layout")
}

fn find_text_abs_y(root: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(node: &FragmentNode, offset: Point, needle: &str) -> Option<f32> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return Some(abs.y);
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, needle) {
        return Some(found);
      }
    }
    None
  }

  walk(root, Point::ZERO, needle)
}

#[test]
fn flex_column_pseudo_element_height_zero_does_not_offset_following_content() {
  let mut renderer = build_renderer();

  // StackOverflow uses `height:0` on flex-item pseudo-elements (e.g. `.s-btn--text:before`) to
  // provide a bolded width probe without affecting height. Ensure we don't treat an authored `0px`
  // size as a Taffy "tiny probe" and incorrectly re-measure to content height.
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #c { display: inline-flex; flex-direction: column; font-size: 12px; line-height: 14px; }
      #c::before { content: "probe"; display: block; height: 0; visibility: hidden; }
    </style>
    <div id="c">Visible</div>
  "##;

  let tree = layout_html(&mut renderer, html);
  let probe_y = find_text_abs_y(&tree.root, "probe").expect("find probe text");
  let visible_y = find_text_abs_y(&tree.root, "Visible").expect("find Visible text");
  assert!(
    (visible_y - probe_y).abs() <= EPS,
    "expected following text to start at same y as height:0 pseudo-element; probe_y={probe_y} visible_y={visible_y}"
  );
}
