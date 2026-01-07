use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;

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
fn container_name_parses_space_separated_custom_idents() {
  let html = r#"
    <style>
      #target { container-name: foo bar; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(
    target.styles.container_name,
    vec!["foo".to_string(), "bar".to_string()]
  );
}

#[test]
fn container_name_rejects_commas() {
  let html = r#"
    <style>
      #target { container-name: foo, bar; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(target.styles.container_name.is_empty());
}

#[test]
fn container_name_rejects_reserved_keywords() {
  let html = r#"
    <style>
      #and { container-name: and; }
      #or { container-name: or; }
      #not { container-name: not; }
    </style>
    <div id="and"></div>
    <div id="or"></div>
    <div id="not"></div>
  "#;

  let styled = styled_tree_for(html);
  for id in ["and", "or", "not"] {
    let node = find_by_id(&styled, id).expect("node");
    assert!(node.styles.container_name.is_empty());
  }
}

#[test]
fn container_name_none_clears_names() {
  let html = r#"
    <style>
      #target { container-name: foo; container-name: none; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(target.styles.container_name.is_empty());
}

#[test]
fn container_name_allows_duplicate_idents() {
  let html = r#"
    <style>
      #target { container-name: foo foo; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(
    target.styles.container_name,
    vec!["foo".to_string(), "foo".to_string()]
  );
}

#[test]
fn container_name_rejects_strings() {
  let html = r#"
    <style>
      #target { container-name: "foo"; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(target.styles.container_name.is_empty());
}

#[test]
fn container_name_none_accepts_comments_and_escapes() {
  let html = r#"
    <style>
      #comment { container-name: foo; container-name: none /*comment*/; }
      #escape { container-name: foo; container-name: n\6fne; }
    </style>
    <div id="comment"></div>
    <div id="escape"></div>
  "#;

  let styled = styled_tree_for(html);
  for id in ["comment", "escape"] {
    let node = find_by_id(&styled, id).expect("node");
    assert!(node.styles.container_name.is_empty());
  }
}

#[test]
fn container_name_rejects_css_wide_keywords_in_lists() {
  let html = r#"
    <style>
      #target { container-name: inherit foo; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(target.styles.container_name.is_empty());
}

#[test]
fn container_name_rejects_default_keyword() {
  let html = r#"
    <style>
      #target { container-name: default; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(target.styles.container_name.is_empty());
}

#[test]
fn container_name_rejects_multiple_none_tokens() {
  let html = r#"
    <style>
      #target { container-name: foo; container-name: none none; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.container_name, vec!["foo".to_string()]);
}
