use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::style::types::{WillChange, WillChangeHint};

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
fn will_change_initial_keyword_resets_to_auto() {
  let html = r#"
    <style>
      #target { will-change: filter; will-change: initial; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.will_change, WillChange::Auto);
}

#[test]
fn will_change_unset_keyword_resets_to_auto() {
  let html = r#"
    <style>
      #parent { will-change: filter; }
      #target { will-change: opacity; will-change: unset; }
    </style>
    <div id="parent">
      <div id="target"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.will_change, WillChange::Auto);
}

#[test]
fn will_change_inherit_keyword_inherits_from_parent() {
  let html = r#"
    <style>
      #parent { will-change: opacity; }
      #target { will-change: filter; will-change: inherit; }
    </style>
    <div id="parent">
      <div id="target"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(
    target.styles.will_change,
    WillChange::Hints(vec![WillChangeHint::Property("opacity".to_string())])
  );
}

#[test]
fn will_change_inherit_keyword_accepts_comments_and_escapes() {
  let html = r#"
    <style>
      #parent { will-change: filter; }
      #target { will-change: in\herit/*comment*/; }
    </style>
    <div id="parent">
      <div id="target"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(
    target.styles.will_change,
    WillChange::Hints(vec![WillChangeHint::Property("filter".to_string())])
  );
}

