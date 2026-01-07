use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{self, DomNode};
use fastrender::style::cascade::{
  apply_styles_with_media_target_and_imports, ContainerQueryContext, ContainerQueryInfo, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::style::types::ContainerType;
use fastrender::style::ComputedStyle;
use fastrender::Rgba;
use std::collections::HashMap;
use std::sync::Arc;

const HTML: &str = r#"<div id="c"></div>"#;

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

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

fn cascade_with_optional_container(css: &str, inline_size: f32, use_container_ctx: bool) -> StyledNode {
  let dom = dom::parse_html(HTML).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);
  let container_node = find_dom_by_id(&dom, "c").expect("container node");
  let container_id = *ids
    .get(&(container_node as *const DomNode))
    .expect("id for container");

  let base_media = MediaContext::screen(800.0, 600.0);
  let container_ctx = if use_container_ctx {
    let mut containers = HashMap::new();
    containers.insert(
      container_id,
      ContainerQueryInfo {
        width: inline_size,
        height: 300.0,
        inline_size,
        block_size: 300.0,
        container_type: ContainerType::InlineSize,
        names: vec![],
        font_size: 16.0,
        styles: Arc::new(ComputedStyle::default()),
      },
    );
    Some(ContainerQueryContext {
      base_media: base_media.clone(),
      containers,
    })
  } else {
    None
  };

  let stylesheet = parse_stylesheet(css).unwrap();
  apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &base_media,
    None,
    None,
    None,
    container_ctx.as_ref(),
    None,
    None,
  )
}

#[test]
fn element_does_not_query_itself() {
  let css = r#"
    #c { display: block; container-type: inline-size; }
    @container (min-width: 0px) {
      #c { display: inline; }
    }
  "#;

  let styled = cascade_with_optional_container(css, 500.0, true);
  assert_eq!(display(find_by_id(&styled, "c").expect("container")), "block");
}

#[test]
fn pseudo_element_can_query_originating_element() {
  let css = r#"
    #c { container-type: inline-size; }
    @container (min-width: 0px) {
      #c::before { content: "x"; color: rgb(1 2 3); }
    }
  "#;

  let styled_without_ctx = cascade_with_optional_container(css, 500.0, false);
  let node_without = find_by_id(&styled_without_ctx, "c").expect("container");
  assert!(
    node_without.before_styles.is_none(),
    "pseudo-element should not be generated without a query container"
  );

  let styled_with_ctx = cascade_with_optional_container(css, 500.0, true);
  let node_with = find_by_id(&styled_with_ctx, "c").expect("container");
  let before = node_with.before_styles.as_ref().expect("before styles");
  assert_eq!(before.color, Rgba::rgb(1, 2, 3));
}
