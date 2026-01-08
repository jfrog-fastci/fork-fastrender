use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::display::Display;
use fastrender::style::media::MediaType;
use fastrender::style::types::Direction;

fn styled_tree_for(html: &str) -> StyledNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
    .styled_tree
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn all_unset_inherits_inherited_properties_and_resets_non_inherited() {
  let html = r#"
    <style>
      #parent { color: rgb(10, 20, 30); }
      #target {
        color: rgb(1, 2, 3);
        display: inline-block;
        direction: rtl;
        all: unset;
      }
    </style>
    <div id="parent">
      <div id="target"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  // `color` is inherited, so `all: unset` should behave like `color: inherit`.
  assert_eq!(target.styles.color, Rgba::rgb(10, 20, 30));
  // `display` is not inherited, so `all: unset` should behave like `display: initial`.
  assert_eq!(target.styles.display, Display::Inline);
  // The `all` property does not apply to direction/unicode-bidi.
  assert_eq!(target.styles.direction, Direction::Rtl);
}

