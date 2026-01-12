use crate::tree::box_tree::{BoxNode, GeneratedPseudoElement};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{Display, FastRender, FontConfig};

const EPS: f32 = 0.01;

fn build_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer")
}

fn find_after_pseudo<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
  if node.generated_pseudo == Some(GeneratedPseudoElement::After) {
    return Some(node);
  }
  for child in &node.children {
    if let Some(found) = find_after_pseudo(child) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_by_box_id<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline {
        box_id: Some(id), ..
      }
      | FragmentContent::Text {
        box_id: Some(id), ..
      }
      | FragmentContent::Replaced {
        box_id: Some(id), ..
      } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn abspos_inline_after_pseudo_is_blockified_and_sized_from_insets() {
  let mut renderer = build_renderer();

  // Regression test for pages like BBC where an `a::after` overlay is `position:absolute` with
  // `top/right/bottom/left:0` but has `display:inline`. Per CSS 2.1 §9.7 / CSS Display Level 3,
  // absolutely positioned boxes are blockified, so the overlay must get a proper block-level box
  // whose used size is solved from the inset constraints (instead of collapsing to 0 height).
  let dom = renderer
    .parse_html(
      r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        #container { position: relative; width: 100px; height: 80px; }
        #container::after {
          content: "";
          position: absolute;
          display: inline;
          top: 0; right: 0; bottom: 0; left: 0;
        }
      </style>
      <div id="container"></div>
    "#,
    )
    .expect("parse HTML");

  let intermediates = renderer
    .layout_document_for_media_intermediates(
      &dom,
      200,
      200,
      crate::style::media::MediaType::Screen,
    )
    .expect("layout document");

  let after = find_after_pseudo(&intermediates.box_tree.root).expect("find ::after box");
  assert!(
    after.is_block_level(),
    "expected absolutely positioned ::after to be blockified to a block-level box; got {after:#?}"
  );
  assert_eq!(
    after.style.display,
    Display::Block,
    "expected blockified display value to be Display::Block"
  );

  let after_fragment =
    find_fragment_by_box_id(&intermediates.fragment_tree.root, after.id).expect("after fragment");
  assert!(
    (after_fragment.bounds.width() - 100.0).abs() <= EPS,
    "expected ::after width to fill its containing block (got {})",
    after_fragment.bounds.width()
  );
  assert!(
    (after_fragment.bounds.height() - 80.0).abs() <= EPS,
    "expected ::after height to fill its containing block (got {})",
    after_fragment.bounds.height()
  );

  // Sanity-check the bounds are finite and non-empty.
  assert!(after_fragment.bounds.width() > 0.0 && after_fragment.bounds.height() > 0.0);
}
