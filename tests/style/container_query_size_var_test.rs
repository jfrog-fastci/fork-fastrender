use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{self, DomNode};
use fastrender::style::cascade::{
  apply_styles_with_media_target_and_imports, ContainerQueryContext, ContainerQueryInfo, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::style::types::ContainerType;
use fastrender::style::values::CustomPropertyValue;
use fastrender::style::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

const HTML_TWO_CONTAINERS: &str = r#"
  <div id="c1" class="container"><div id="t1" class="target"></div></div>
  <div id="c2" class="container"><div id="t2" class="target"></div></div>
"#;

const HTML_ONE_CONTAINER: &str = r#"
  <div id="c1" class="container"><div id="t1" class="target"></div></div>
"#;

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_dom_by_id(child, id))
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

fn make_container_style(query_value: Option<&str>) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  if let Some(value) = query_value {
    style.custom_properties.insert(
      "--query".into(),
      CustomPropertyValue::new(value.to_string(), None),
    );
  }
  Arc::new(style)
}

#[test]
fn container_size_query_var_resolves_per_container() {
  let css = r#"
    .target { display: block; }
    @container (width > var(--query)) {
      .target { display: inline; }
    }
  "#;

  let dom = dom::parse_html(HTML_TWO_CONTAINERS).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);
  let container1 = find_dom_by_id(&dom, "c1").expect("container 1");
  let container2 = find_dom_by_id(&dom, "c2").expect("container 2");
  let c1_id = *ids.get(&(container1 as *const DomNode)).expect("id c1");
  let c2_id = *ids.get(&(container2 as *const DomNode)).expect("id c2");

  let base_media = MediaContext::screen(800.0, 600.0);
  let mut containers = HashMap::new();
  containers.insert(
    c1_id,
    ContainerQueryInfo {
      inline_size: 200.0,
      block_size: 300.0,
      container_type: ContainerType::InlineSize,
      names: Vec::new(),
      font_size: 16.0,
      styles: make_container_style(Some("150px")),
    },
  );
  containers.insert(
    c2_id,
    ContainerQueryInfo {
      inline_size: 200.0,
      block_size: 300.0,
      container_type: ContainerType::InlineSize,
      names: Vec::new(),
      font_size: 16.0,
      styles: make_container_style(Some("250px")),
    },
  );
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };
  let stylesheet = parse_stylesheet(css).unwrap();

  let styled = apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &base_media,
    None,
    None,
    None,
    Some(&ctx),
    None,
    None,
  );

  assert_eq!(display(find_by_id(&styled, "t1").expect("target 1")), "inline");
  assert_eq!(display(find_by_id(&styled, "t2").expect("target 2")), "block");
}

#[test]
fn container_size_query_var_fallback_used_when_missing() {
  let css = r#"
    .target { display: block; }
    @container (width > var(--missing, 150px)) {
      .target { display: inline; }
    }
  "#;

  let dom = dom::parse_html(HTML_ONE_CONTAINER).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);
  let container = find_dom_by_id(&dom, "c1").expect("container");
  let container_id = *ids.get(&(container as *const DomNode)).expect("id for container");

  let base_media = MediaContext::screen(800.0, 600.0);
  let mut containers = HashMap::new();
  containers.insert(
    container_id,
    ContainerQueryInfo {
      inline_size: 200.0,
      block_size: 300.0,
      container_type: ContainerType::InlineSize,
      names: Vec::new(),
      font_size: 16.0,
      styles: Arc::new(ComputedStyle::default()),
    },
  );
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };
  let stylesheet = parse_stylesheet(css).unwrap();

  let styled = apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &base_media,
    None,
    None,
    None,
    Some(&ctx),
    None,
    None,
  );

  assert_eq!(display(find_by_id(&styled, "t1").expect("target")), "inline");
}

