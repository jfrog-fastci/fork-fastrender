use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::FastRender;

fn find_replaced_image<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
    if matches!(replaced_type, ReplacedType::Image { .. }) {
      return Some(node);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_replaced_image(child) {
      return Some(found);
    }
  }

  None
}

#[test]
fn replaced_block_respects_writing_mode_axes() {
  // In vertical writing modes the inline axis is vertical and the block axis is horizontal.
  // The physical size of an image should remain width×height, even though inline/block axes swap.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container { writing-mode: vertical-rl; }
          img { display: block; width: 40px; height: 60px; }
        </style>
      </head>
      <body>
        <div class="container">
          <img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=">
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let img = find_replaced_image(&tree.root).expect("expected replaced image fragment");
  let epsilon = 0.01;
  assert!(
    (img.bounds.width() - 40.0).abs() < epsilon,
    "expected img physical width to remain 40px; got {:?}",
    img.bounds
  );
  assert!(
    (img.bounds.height() - 60.0).abs() < epsilon,
    "expected img physical height to remain 60px; got {:?}",
    img.bounds
  );
}

#[test]
fn replaced_inline_respects_writing_mode_axes() {
  // Inline-level replaced elements are sized in the inline axis, which is vertical in
  // `writing-mode: vertical-rl`.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container { writing-mode: vertical-rl; font-size: 0; }
          img { width: 40px; height: 60px; }
        </style>
      </head>
      <body>
        <div class="container">
          <img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=">
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let img = find_replaced_image(&tree.root).expect("expected replaced image fragment");
  let epsilon = 0.01;
  assert!(
    (img.bounds.width() - 40.0).abs() < epsilon,
    "expected img physical width to remain 40px; got {:?}",
    img.bounds
  );
  assert!(
    (img.bounds.height() - 60.0).abs() < epsilon,
    "expected img physical height to remain 60px; got {:?}",
    img.bounds
  );
}
