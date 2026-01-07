use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{self, DomNode};
use fastrender::style::cascade::{
  apply_styles_with_media_target_and_imports, ContainerQueryContext, ContainerQueryInfo, StyledNode,
};
use fastrender::style::content::ContentValue;
use fastrender::style::media::MediaContext;
use fastrender::style::types::ContainerType;
use fastrender::style::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

const HTML: &str = r#"<div id="container"><div id="target"></div></div>"#;
const HTML_MARKER: &str = r#"<div id="container"><ul><li id="target">Item</li></ul></div>"#;

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_dom_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
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

fn cascade_with_container_inline_size(inline_size: f32) -> StyledNode {
  let dom = dom::parse_html(HTML).expect("parse html");
  let ids = dom::enumerate_dom_ids(&dom);
  let container_node = find_dom_by_id(&dom, "container").expect("container node");
  let container_id = *ids
    .get(&(container_node as *const DomNode))
    .expect("id for container");

  let css = format!(
    r#"
      #container {{ container-type: inline-size; width: {inline_size}px; }}
      #target::before {{ content: "base"; }}
      @container (min-width: 150px) {{
        #target::before {{ content: "cq"; }}
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("parse stylesheet");
  let base_media = MediaContext::screen(800.0, 600.0);
  let containers = HashMap::from([(
    container_id,
    ContainerQueryInfo {
      inline_size,
      block_size: 300.0,
      container_type: ContainerType::InlineSize,
      names: Vec::new(),
      font_size: 16.0,
      styles: Arc::new(ComputedStyle::default()),
    },
  )]);
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };

  apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &base_media,
    None,
    None,
    None,
    Some(&ctx),
    None,
    None,
  )
}

fn cascade_marker_with_container_inline_size(inline_size: f32) -> StyledNode {
  let dom = dom::parse_html(HTML_MARKER).expect("parse html");
  let ids = dom::enumerate_dom_ids(&dom);
  let container_node = find_dom_by_id(&dom, "container").expect("container node");
  let container_id = *ids
    .get(&(container_node as *const DomNode))
    .expect("id for container");

  let css = format!(
    r#"
      #container {{ container-type: inline-size; width: {inline_size}px; }}
      #target::marker {{ content: "base"; }}
      @container (min-width: 150px) {{
        #target::marker {{ content: "cq"; }}
      }}
    "#
  );
  let stylesheet = parse_stylesheet(&css).expect("parse stylesheet");
  let base_media = MediaContext::screen(800.0, 600.0);
  let containers = HashMap::from([(
    container_id,
    ContainerQueryInfo {
      inline_size,
      block_size: 300.0,
      container_type: ContainerType::InlineSize,
      names: Vec::new(),
      font_size: 16.0,
      styles: Arc::new(ComputedStyle::default()),
    },
  )]);
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };

  apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &base_media,
    None,
    None,
    None,
    Some(&ctx),
    None,
    None,
  )
}

#[test]
fn pseudo_element_container_query_filters_unmatched_rules() {
  let styled = cascade_with_container_inline_size(100.0);
  let target = find_by_id(&styled, "target").expect("target element");
  let before = target.before_styles.as_ref().expect("before styles");
  assert_eq!(before.content_value, ContentValue::from_string("base"));
}

#[test]
fn pseudo_element_container_query_applies_when_matched() {
  let styled = cascade_with_container_inline_size(200.0);
  let target = find_by_id(&styled, "target").expect("target element");
  let before = target.before_styles.as_ref().expect("before styles");
  assert_eq!(before.content_value, ContentValue::from_string("cq"));
}

#[test]
fn marker_pseudo_element_container_query_filters_unmatched_rules() {
  let styled = cascade_marker_with_container_inline_size(100.0);
  let target = find_by_id(&styled, "target").expect("target element");
  let marker = target.marker_styles.as_ref().expect("marker styles");
  assert_eq!(marker.content_value, ContentValue::from_string("base"));
}

#[test]
fn marker_pseudo_element_container_query_applies_when_matched() {
  let styled = cascade_marker_with_container_inline_size(200.0);
  let target = find_by_id(&styled, "target").expect("target element");
  let marker = target.marker_styles.as_ref().expect("marker styles");
  assert_eq!(marker.content_value, ContentValue::from_string("cq"));
}
