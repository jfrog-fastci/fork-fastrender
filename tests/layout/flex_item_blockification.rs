use fastrender::geometry::Point;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{Display, FastRender, FontConfig};

const EPS: f32 = 0.01;

fn build_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer")
}

fn layout_html(renderer: &mut FastRender, html: &str) -> fastrender::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer.layout_document(&dom, 200, 200).expect("layout")
}

fn find_block_y_for_text(root: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(
    node: &FragmentNode,
    offset: Point,
    current_block_y: Option<f32>,
    needle: &str,
  ) -> Option<f32> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    let current_block_y = if matches!(node.content, FragmentContent::Block { .. }) {
      Some(abs.y)
    } else {
      current_block_y
    };

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return current_block_y;
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, current_block_y, needle) {
        return Some(found);
      }
    }
    None
  }

  walk(root, Point::ZERO, None, needle)
}

fn find_box_with_id<'a>(node: &'a BoxNode, id: &str) -> Option<&'a BoxNode> {
  if node.debug_info.as_ref().and_then(|info| info.id.as_deref()) == Some(id) {
    return Some(node);
  }
  for child in &node.children {
    if let Some(found) = find_box_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn flex_items_blockify_inline_elements_with_block_children() {
  let mut renderer = build_renderer();

  // Per CSS Display/Flexbox, flex items are blockified: an `a { display: inline }` direct child of
  // a flex container must behave as a block-level flex item. If we fail to blockify, anonymous box
  // fixup can split the inline around its block children, effectively dropping the wrapper and its
  // margins from layout.
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #container { display: flex; flex-direction: column; }
      a { margin: 0 0 10px 0; }
      .inner { display: block; height: 20px; }
    </style>
    <div id="container">
      <a href="#"><div class="inner">A</div></a>
      <a href="#"><div class="inner">B</div></a>
    </div>
  "##;

  let tree = layout_html(&mut renderer, html);

  let a_y = find_block_y_for_text(&tree.root, "A").expect("find A");
  let b_y = find_block_y_for_text(&tree.root, "B").expect("find B");
  let delta = b_y - a_y;

  assert!(
    (delta - 30.0).abs() <= EPS,
    "expected B to be positioned after A plus the flex item's bottom margin (delta={delta})"
  );
}

#[test]
fn flex_items_blockify_inline_level_elements() {
  let mut renderer = build_renderer();

  let dom = renderer
    .parse_html(
      r#"
      <div style="display:flex">
        <div style="display:contents">
          <a id="item" style="padding: 10px">Hello</a>
        </div>
        <a id="abs" style="position:absolute; left: 0; top: 0">Abs</a>
      </div>
    "#,
    )
    .expect("parse HTML");

  let intermediates = renderer
    .layout_document_for_media_intermediates(
      &dom,
      300,
      200,
      fastrender::style::media::MediaType::Screen,
    )
    .expect("layout document");

  let item = find_box_with_id(&intermediates.box_tree.root, "item").expect("find flex item");
  assert!(
    item.is_block_level(),
    "expected flex item with inline display to be blockified to a block-level box; got {item:#?}"
  );
  assert_eq!(
    item.style.display,
    Display::Block,
    "expected blockified display value to be Display::Block"
  );

  let abs = find_box_with_id(&intermediates.box_tree.root, "abs").expect("find abspos item");
  assert!(
    abs.is_block_level(),
    "expected absolutely positioned element to be blockified to a block-level box; got {abs:#?}"
  );
  assert_eq!(
    abs.style.display,
    Display::Block,
    "expected absolutely positioned element's used display value to be blockified to Display::Block"
  );
}
