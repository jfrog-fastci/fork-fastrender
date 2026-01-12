use fastrender::style::display::Display;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::FastRender;

fn find_inline_block<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| matches!(style.display, Display::InlineBlock))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_inline_block(child) {
      return Some(found);
    }
  }

  None
}

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
fn inline_block_respects_writing_mode_axes() {
  // Inline-blocks are atomic in the inline formatting context; in vertical writing modes the
  // inline axis is vertical while the block axis is horizontal. The box's physical size should
  // remain width×height, and its contents should also be laid out in physical coordinates.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container { writing-mode: vertical-rl; font-size: 0; }
          .ib { display: inline-block; }
          img { width: 40px; height: 60px; display: block; }
        </style>
      </head>
      <body>
        <div class="container">
          <span class="ib">
            <img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=">
          </span>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let inline_block = find_inline_block(&tree.root).expect("expected inline-block fragment");
  let img = find_replaced_image(&tree.root).expect("expected replaced image fragment");

  let epsilon = 0.01;
  assert!(
    (inline_block.bounds.width() - 40.0).abs() < epsilon,
    "expected inline-block physical width to remain 40px; got {:?}",
    inline_block.bounds
  );
  assert!(
    (inline_block.bounds.height() - 60.0).abs() < epsilon,
    "expected inline-block physical height to remain 60px; got {:?}",
    inline_block.bounds
  );

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

  assert!(
    img.bounds.min_x() + epsilon >= inline_block.bounds.min_x()
      && img.bounds.max_x() <= inline_block.bounds.max_x() + epsilon
      && img.bounds.min_y() + epsilon >= inline_block.bounds.min_y()
      && img.bounds.max_y() <= inline_block.bounds.max_y() + epsilon,
    "expected img fragment to be contained within inline-block; inline-block={:?} img={:?}",
    inline_block.bounds,
    img.bounds
  );
}
