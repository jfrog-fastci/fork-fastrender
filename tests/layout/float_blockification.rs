use fastrender::tree::box_tree::BoxNode;
use fastrender::{Display, FastRender, FontConfig};

fn build_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer")
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
fn floats_blockify_inline_elements_with_block_children() {
  let mut renderer = build_renderer();

  // Per CSS 2.1 §9.7, floats are blockified: a `div { float:left; display:inline }` must generate a
  // block-level float box. Without blockification, box tree fixups treat block descendants as
  // "block-in-inline" and split/hoist them out of the floated box, dropping the wrapper from
  // layout.
  let dom = renderer
    .parse_html(
      r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        #float { float: left; display: inline; }
        #inner { display: block; height: 20px; }
      </style>
      <div id="float">
        <div id="inner">inner</div>
      </div>
    "#,
    )
    .expect("parse HTML");

  let intermediates = renderer
    .layout_document_for_media_intermediates(
      &dom,
      200,
      200,
      fastrender::style::media::MediaType::Screen,
    )
    .expect("layout document");

  let float = find_box_with_id(&intermediates.box_tree.root, "float").expect("find float box");
  assert!(
    float.is_block_level(),
    "expected floated inline element to be blockified; got {float:#?}"
  );
  assert_eq!(
    float.style.display,
    Display::Block,
    "expected blockified display value to be Display::Block"
  );

  let _inner =
    find_box_with_id(float, "inner").expect("expected block child to remain inside float");
}
