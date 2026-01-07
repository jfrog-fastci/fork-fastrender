use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{self, DomNode};
use fastrender::style::cascade::{
  apply_styles_with_media_target_and_imports, ContainerQueryContext, ContainerQueryInfo, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::style::types::{ContainerType, WritingMode};
use fastrender::style::values::CustomPropertyValue;
use fastrender::style::ComputedStyle;
use std::collections::HashMap;
use std::sync::Arc;

const HTML: &str = r#"<div id="c" class="container"><div id="t" class="target"></div></div>"#;

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

fn cascade_with_container_styles(
  css: &str,
  inline_size: f32,
  names: Vec<String>,
  styles: Arc<ComputedStyle>,
) -> StyledNode {
  let dom = dom::parse_html(HTML).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);
  let container_node = find_dom_by_id(&dom, "c").expect("container node");
  let container_id = *ids
    .get(&(container_node as *const DomNode))
    .expect("id for container");

  let base_media = MediaContext::screen(800.0, 600.0);
  let mut containers = HashMap::new();

  // Ensure the container metadata matches the computed style used by container selection logic.
  let mut style = (*styles).clone();
  style.container_type = ContainerType::InlineSize;
  let writing_mode = style.writing_mode;
  let styles = Arc::new(style);

  let block_size = 300.0;
  let (width, height) = match writing_mode {
    WritingMode::HorizontalTb => (inline_size, block_size),
    _ => (block_size, inline_size),
  };

  containers.insert(
    container_id,
    ContainerQueryInfo {
      width,
      height,
      inline_size,
      block_size,
      container_type: ContainerType::InlineSize,
      names,
      font_size: styles.font_size,
      styles,
    },
  );
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };
  let stylesheet = parse_stylesheet(css).unwrap();

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

fn cascade_with_container(css: &str, inline_size: f32, names: Vec<String>) -> StyledNode {
  cascade_with_container_styles(css, inline_size, names, Arc::new(ComputedStyle::default()))
}

fn cascade_with_custom_container(
  css: &str,
  width: f32,
  height: f32,
  writing_mode: WritingMode,
  container_type: ContainerType,
  names: Vec<String>,
) -> StyledNode {
  let dom = dom::parse_html(HTML).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);
  let container_node = find_dom_by_id(&dom, "c").expect("container node");
  let container_id = *ids
    .get(&(container_node as *const DomNode))
    .expect("id for container");

  let base_media = MediaContext::screen(800.0, 600.0);
  let mut containers = HashMap::new();
  let (inline_size, block_size) = match writing_mode {
    WritingMode::HorizontalTb => (width, height),
    _ => (height, width),
  };
  let mut style = ComputedStyle::default();
  style.container_type = container_type;
  style.writing_mode = writing_mode;
  let styles = Arc::new(style);
  containers.insert(
    container_id,
    ContainerQueryInfo {
      width,
      height,
      inline_size,
      block_size,
      container_type,
      names,
      font_size: styles.font_size,
      styles,
    },
  );
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers,
  };
  let stylesheet = parse_stylesheet(css).unwrap();

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

fn cascade_with_containers(
  html: &str,
  css: &str,
  containers: Vec<(&str, ContainerQueryInfo)>,
) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let ids = dom::enumerate_dom_ids(&dom);

  let base_media = MediaContext::screen(800.0, 600.0);
  let mut container_map = HashMap::new();
  for (id, info) in containers {
    let node = find_dom_by_id(&dom, id).expect("container node");
    let node_id = *ids.get(&(node as *const DomNode)).expect("id for container");
    container_map.insert(node_id, info);
  }
  let ctx = ContainerQueryContext {
    base_media: base_media.clone(),
    containers: container_map,
  };
  let stylesheet = parse_stylesheet(css).unwrap();

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
fn container_query_applies_when_size_matches() {
  let css = r#"
    @container (min-width: 400px) {
      .target { display: inline; }
    }
  "#;
  let styled = cascade_with_container(css, 500.0, vec![]);

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn not_container_query_parses_and_evaluates() {
  let css = r#"
    .target { display: block; }
    @container not (min-width: 400px) {
      .target { display: inline; }
    }
  "#;

  let styled_small = cascade_with_container(css, 300.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_small, "t").expect("target")),
    "inline"
  );

  let styled_large = cascade_with_container(css, 500.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_large, "t").expect("target")),
    "block"
  );
}

#[test]
fn container_query_or_with_unknown_branch_matches_when_known_branch_true() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 400px) or scroll-state(stuck: top) {
      .target { display: inline; }
    }
  "#;
  let styled = cascade_with_container(css, 500.0, vec![]);

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn not_unknown_container_query_does_not_match() {
  let css = r#"
    .target { display: block; }
    @container not scroll-state(stuck: top) {
      .target { display: inline; }
    }
  "#;
  let styled = cascade_with_container(css, 500.0, vec![]);

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn named_container_selection() {
  let css = r#"
    .target { display: block; }
    @container sidebar (min-width: 400px) {
      .target { display: inline; }
    }
  "#;

  let styled_named = cascade_with_container(css, 500.0, vec!["sidebar".into()]);
  assert_eq!(
    display(find_by_id(&styled_named, "t").expect("target")),
    "inline"
  );

  let styled_unnamed = cascade_with_container(css, 500.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_unnamed, "t").expect("target")),
    "block"
  );
}

#[test]
fn container_query_accepts_ident_function_container_name() {
  let css = r#"
    .target { display: block; }
    @container ident(sidebar) (min-width: 400px) {
      .target { display: inline; }
    }
  "#;

  let styled_named = cascade_with_container(css, 500.0, vec!["sidebar".into()]);
  assert_eq!(
    display(find_by_id(&styled_named, "t").expect("target")),
    "inline"
  );
}

#[test]
fn container_query_list_uses_or_semantics() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 600px), (max-width: 200px) {
      .target { display: inline; }
    }
  "#;

  let styled_wide = cascade_with_container(css, 650.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_wide, "t").expect("target")),
    "inline"
  );

  let styled_narrow = cascade_with_container(css, 150.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_narrow, "t").expect("target")),
    "inline"
  );

  let styled_middle = cascade_with_container(css, 300.0, vec![]);
  assert_eq!(
    display(find_by_id(&styled_middle, "t").expect("target")),
    "block"
  );
}

#[test]
fn container_query_rejects_reserved_container_names() {
  let css = r#"
    .target { display: block; }
    @container and (min-width: 0px) {
      .target { display: inline; }
    }
  "#;

  let styled = cascade_with_container(css, 500.0, vec!["and".into()]);
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn container_query_rem_uses_root_font_size() {
  let css = r#"
    @container (min-width: 12rem) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 20.0;
  style.root_font_size = 10.0;

  let styled = cascade_with_container_styles(css, 150.0, vec![], Arc::new(style));

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_em_uses_container_font_size() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 8em) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 20.0;
  style.root_font_size = 10.0;

  let styled = cascade_with_container_styles(css, 150.0, vec![], Arc::new(style));

  // 8em = 160px when resolved against the query container's computed 20px font size.
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn container_query_calc_rem_uses_root_font_size() {
  let css = r#"
    .target { display: block; }
    @container (min-width: calc(12rem - 2em)) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 20.0;
  style.root_font_size = 10.0;

  let styled = cascade_with_container_styles(css, 150.0, vec![], Arc::new(style));

  // 12rem - 2em = 120px - 40px = 80px, so the query should match.
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_calc_em_uses_container_font_size() {
  let css = r#"
    .target { display: block; }
    @container (min-width: calc(8em + 1px)) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 20.0;
  style.root_font_size = 10.0;

  let styled = cascade_with_container_styles(css, 150.0, vec![], Arc::new(style));

  // 8em + 1px = 160px + 1px, so the query should *not* match.
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn container_query_em_respects_zero_font_size() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 1em) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 0.0;
  style.root_font_size = 10.0;

  let styled = cascade_with_container_styles(css, 10.0, vec![], Arc::new(style));

  // 1em resolves to 0px when the query container's computed font size is 0px, so the query matches.
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_rem_respects_zero_root_font_size() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 1rem) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.font_size = 20.0;
  style.root_font_size = 0.0;

  let styled = cascade_with_container_styles(css, 10.0, vec![], Arc::new(style));

  // 1rem resolves to 0px when the root element's computed font size is 0px, so the query matches.
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_comma_conditions_select_independent_containers() {
  let html = r#"
    <div id="outer">
      <div id="inner">
        <div id="t" class="target"></div>
      </div>
    </div>
  "#;
  let css = r#"
    .target { display: block; }
    @container card (min-width: 999px), style(--large: true) {
      .target { display: inline; }
    }
  "#;

  let mut inner_styles = ComputedStyle::default();
  inner_styles.custom_properties.insert(
    Arc::from("--large"),
    CustomPropertyValue::new("true", None),
  );
  let inner_styles = Arc::new(inner_styles);
  let outer_styles = Arc::new(ComputedStyle::default());

  let styled = cascade_with_containers(
    html,
    css,
    vec![
      (
        "outer",
        ContainerQueryInfo {
          width: 500.0,
          height: 300.0,
          inline_size: 500.0,
          block_size: 300.0,
          container_type: ContainerType::InlineSize,
          names: vec!["card".into()],
          font_size: outer_styles.font_size,
          styles: Arc::clone(&outer_styles),
        },
      ),
      (
        "inner",
        ContainerQueryInfo {
          width: 0.0,
          height: 0.0,
          inline_size: 0.0,
          block_size: 0.0,
          container_type: ContainerType::Normal,
          names: Vec::new(),
          font_size: inner_styles.font_size,
          styles: Arc::clone(&inner_styles),
        },
      ),
    ],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_comma_conditions_allow_distinct_names() {
  let html = r#"
    <div id="foo">
      <div id="bar">
        <div id="t" class="target"></div>
      </div>
    </div>
  "#;
  let css = r#"
    .target { display: block; }
    @container foo (min-width: 400px), bar (min-width: 200px) {
      .target { display: inline; }
    }
  "#;

  let styled = cascade_with_containers(
    html,
    css,
    vec![
      (
        "foo",
        ContainerQueryInfo {
          width: 300.0,
          height: 300.0,
          inline_size: 300.0,
          block_size: 300.0,
          container_type: ContainerType::InlineSize,
          names: vec!["foo".into()],
          font_size: 16.0,
          styles: Arc::new(ComputedStyle::default()),
        },
      ),
      (
        "bar",
        ContainerQueryInfo {
          width: 250.0,
          height: 300.0,
          inline_size: 250.0,
          block_size: 300.0,
          container_type: ContainerType::InlineSize,
          names: vec!["bar".into()],
          font_size: 16.0,
          styles: Arc::new(ComputedStyle::default()),
        },
      ),
    ],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_name_only_condition_matches_when_container_exists() {
  let css = r#"
     .target { display: block; }
     @container sidebar {
       .target { display: inline; }
     }
   "#;

  let styled = cascade_with_container(css, 500.0, vec!["sidebar".into()]);
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_distinguishes_width_and_inline_size_in_vertical_writing_mode() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 150px) {
      .target { display: inline; }
    }
    @container (min-inline-size: 150px) {
      .target { display: flex; }
    }
  "#;

  let styled = cascade_with_custom_container(
    css,
    200.0,
    100.0,
    WritingMode::VerticalRl,
    ContainerType::Size,
    vec![],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_orientation_and_aspect_ratio_use_physical_axes() {
  let css = r#"
    .target { display: block; }
    @container (orientation: landscape) and (min-aspect-ratio: 3/2) {
      .target { display: inline; }
    }
  "#;

  let styled = cascade_with_custom_container(
    css,
    200.0,
    100.0,
    WritingMode::VerticalRl,
    ContainerType::Size,
    vec![],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn not_container_query_with_invalid_var_length_does_not_match() {
  let css = r#"
    .target { display: block; }
    @container not (min-width: var(--bad)) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.custom_properties.insert(
    Arc::from("--bad"),
    CustomPropertyValue::new("foo", None),
  );

  let styled = cascade_with_container_styles(css, 500.0, vec![], Arc::new(style));
  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn not_container_query_with_unknown_block_size_does_not_match() {
  let css = r#"
    .target { display: block; }
    @container not (min-height: 1px) {
      .target { display: inline; }
    }
  "#;

  let mut style = ComputedStyle::default();
  style.container_type = ContainerType::Size;
  let styles = Arc::new(style);

  let styled = cascade_with_containers(
    HTML,
    css,
    vec![(
      "c",
      ContainerQueryInfo {
        width: 500.0,
        height: f32::NAN,
        inline_size: 500.0,
        block_size: f32::NAN,
        container_type: ContainerType::Size,
        names: Vec::new(),
        font_size: styles.font_size,
        styles,
      },
    )],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}

#[test]
fn container_query_resolves_container_units_in_size_features() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 50cqw) {
      .target { display: inline; }
    }
  "#;

  let styled = cascade_with_custom_container(
    css,
    200.0,
    100.0,
    WritingMode::HorizontalTb,
    ContainerType::Size,
    vec![],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_resolves_container_units_inside_calc_in_size_features() {
  let css = r#"
    .target { display: block; }
    @container (min-width: calc(50cqw + 1px)) {
      .target { display: inline; }
    }
  "#;

  let styled = cascade_with_custom_container(
    css,
    200.0,
    100.0,
    WritingMode::HorizontalTb,
    ContainerType::Size,
    vec![],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "inline");
}

#[test]
fn container_query_viewport_units_use_media_viewport() {
  let css = r#"
    .target { display: block; }
    @container (min-width: 50vw) {
      .target { display: inline; }
    }
  "#;

  // Base media viewport width is 800px (see helper), so 50vw = 400px. The container width is only
  // 300px, so the query should not match.
  let styled = cascade_with_custom_container(
    css,
    300.0,
    200.0,
    WritingMode::HorizontalTb,
    ContainerType::Size,
    vec![],
  );

  assert_eq!(display(find_by_id(&styled, "t").expect("target")), "block");
}
