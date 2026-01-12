use fastrender::geometry::Point;
use fastrender::style::color::Rgba;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

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
) -> fastrender::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer.layout_document(&dom, 200, 200).expect("layout")
}

fn find_block_abs_y_and_height_with_background(
  root: &FragmentNode,
  bg: Rgba,
) -> Option<(f32, f32)> {
  fn walk(node: &FragmentNode, offset: Point, bg: Rgba) -> Option<(f32, f32)> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    if matches!(node.content, FragmentContent::Block { .. }) {
      if node
        .style
        .as_ref()
        .is_some_and(|style| style.background_color == bg)
      {
        return Some((abs.y, node.bounds.height()));
      }
    }

    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, bg) {
        return Some(found);
      }
    }
    None
  }

  walk(root, Point::ZERO, bg)
}

#[test]
fn inline_border_padding_does_not_increase_line_box_height() {
  let mut renderer = build_renderer();

  // Non-replaced inline borders/padding should be "ink overflow" and must not increase the
  // containing line box height (Chrome behavior; required for `xkcd.com` nav buttons).
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #one, #two { margin: 0; font-size: 16px; line-height: 20px; }
      #one { background: red; }
      #two { background: blue; }
      #inline { border: 4px solid black; padding: 4px 0; }
    </style>
    <div id="one">Hello<span id="inline">X</span></div>
    <div id="two">World</div>
  "##;

  let tree = layout_html(&mut renderer, html);
  let (y_one, h_one) = find_block_abs_y_and_height_with_background(&tree.root, Rgba::RED)
    .expect("find block with red background");
  let (y_two, _) = find_block_abs_y_and_height_with_background(&tree.root, Rgba::BLUE)
    .expect("find block with blue background");

  assert!(
    (h_one - 20.0).abs() <= EPS,
    "expected first block height to equal line-height; got {h_one}"
  );
  assert!(
    (y_two - (y_one + 20.0)).abs() <= EPS,
    "expected second block to start immediately after first line box; y_one={y_one} y_two={y_two}"
  );
}

