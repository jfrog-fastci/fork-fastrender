use crate::geometry::Rect;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_block_with_line_children_and_width<'a>(
  node: &'a FragmentNode,
  target_width: f32,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && (node.bounds.width() - target_width).abs() < 0.5
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }
  for child in &node.children {
    if let Some(found) = find_block_with_line_children_and_width(child, target_width) {
      return Some(found);
    }
  }
  None
}

fn line_bounds_for(html: &str, block_width: f32) -> Vec<Rect> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 600, 400)
    .expect("layout document");

  let block = find_block_with_line_children_and_width(&fragments.root, block_width)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| line.bounds)
    .collect()
}

fn find_block_fragment_by_size<'a>(
  node: &'a FragmentNode,
  width: f32,
  height: f32,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && (node.bounds.width() - width).abs() < 0.5
    && (node.bounds.height() - height).abs() < 0.5
  {
    return Some(node);
  }
  for child in &node.children {
    if let Some(found) = find_block_fragment_by_size(child, width, height) {
      return Some(found);
    }
  }
  None
}

#[test]
fn phoronix_like_float_padding_shifts_line_boxes() {
  // Phoronix's home page uses thumbnail images floated left with padding. The text should wrap
  // next to the float's *border box* (including padding), not just the specified width.
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:300px;}\
    .thumb{float:left;width:180px;height:20px;padding:0 12px 0 4px;background:#00f;}\
    .para{font-size:0;line-height:10px;}\
    .piece{display:inline-block;width:50px;height:10px;background:#0f0;vertical-align:top;}\
  </style>\
  <div id=wrap>\
    <div class=thumb></div>\
    <div class=para>\
      <span class=piece></span><span class=piece></span><span class=piece></span><span class=piece></span><span class=piece></span>\
    </div>\
  </div>";

  let lines = line_bounds_for(html, 300.0);
  assert_eq!(lines.len(), 3);

  // Float border-box width = 180 + (4 + 12) = 196.
  assert!((lines[0].x() - 196.0).abs() < 0.5);
  assert!((lines[0].y() - 0.0).abs() < 0.5);
  assert!((lines[0].width() - 104.0).abs() < 0.5);

  assert!((lines[1].x() - 196.0).abs() < 0.5);
  assert!((lines[1].y() - 10.0).abs() < 0.5);
  assert!((lines[1].width() - 104.0).abs() < 0.5);

  // Third line begins after the float ends at y=20.
  assert!((lines[2].x() - 0.0).abs() < 0.5);
  assert!((lines[2].y() - 20.0).abs() < 0.5);
  assert!((lines[2].width() - 300.0).abs() < 0.5);
}

#[test]
fn phoronix_like_two_column_floats_with_negative_margin() {
  // Phoronix uses two column floats where the right column has `margin-left:-1px` and
  // `border-left:1px` to draw a divider line without introducing an extra gap.
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:400px;}\
    #main{float:left;width:250px;height:10px;background:#00f;}\
    #sidebar{float:left;width:150px;height:10px;margin-left:-1px;border-left:1px solid #000;background:#0f0;}\
  </style>\
  <div id=wrap><div id=main></div><div id=sidebar></div></div>";

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 600, 200)
    .expect("layout document");

  // #sidebar border box width = 150 + border-left(1) = 151.
  let sidebar = find_block_fragment_by_size(&fragments.root, 151.0, 10.0)
    .expect("expected sidebar float fragment");

  // Floats are placed next to each other using their margin boxes. With margin-left:-1px, the
  // sidebar's border box should be shifted 1px left relative to the main column's width (250px).
  assert!(
    (sidebar.bounds.x() - 249.0).abs() < 0.5,
    "expected sidebar to be positioned at x=249, got {:.2}",
    sidebar.bounds.x()
  );
  assert!(
    (sidebar.bounds.max_x() - 400.0).abs() < 0.5,
    "expected sidebar to end at x=400, got {:.2}",
    sidebar.bounds.max_x()
  );
}
