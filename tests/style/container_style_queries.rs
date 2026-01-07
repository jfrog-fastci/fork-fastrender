use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaType;
use fastrender::style::types::ContainerType;

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
fn container_style_query_matches_custom_property() {
  let html = r#"
    <style>
      .container { container-type: inline-size; container-name: demo; --foo: bar; }
      .child { color: rgb(0 0 255); }
      @container demo style(--foo: bar) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div id="container" class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_matches_without_explicit_container_type() {
  let html = r#"
    <style>
      .container { --foo: bar; }
      .child { color: rgb(0 0 255); }
      @container style(--foo: bar) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div id="container" class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_falls_back_when_variable_unset() {
  let html = r#"
    <style>
      .container { container-type: inline-size; }
      .child { color: rgb(0 0 255); }
      @container style(--missing: present) {
        .child { color: rgb(5 6 7); }
      }
    </style>
    <div class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(0, 0, 255));
}

#[test]
fn container_style_query_interacts_with_layers_and_important() {
  let html = r#"
    <style>
      @layer defaults, utilities;
      @layer defaults {
        .child { color: rgb(0 128 0) !important; }
      }
      @layer utilities {
        @container style(--theme: dark) {
          .child { color: rgb(128 0 128); }
        }
      }
      .container { container-type: inline-size; --theme: dark; }
    </style>
    <div class="container">
      <div id="layered" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "layered").expect("layered element");
  assert_eq!(target.styles.color, Rgba::rgb(0, 128, 0));
}

#[test]
fn nested_containers_use_nearest_style_query_match() {
  let html = r#"
    <style>
      .outer { container-type: inline-size; --theme: outer; }
      .inner { container-type: inline-size; --theme: inner; }
      #nested { color: black; }
      @container style(--theme: outer) {
        #nested { color: rgb(255 105 180); }
      }
      @container style(--theme: inner) {
        #nested { color: rgb(255 165 0); }
      }
    </style>
    <div class="outer">
      <div class="inner">
        <div id="nested">hello</div>
      </div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "nested").expect("nested element");
  assert_eq!(target.styles.color, Rgba::rgb(255, 165, 0));
}

#[test]
fn container_type_rejects_none_and_style_keywords_and_container_shorthand_resets_to_normal() {
  let html = r#"
    <style>
      .base { container-type: inline-size; }
      .invalid-style { container-type: style; }
      .invalid-none { container-type: none; }
      .commented-size { container-type: size/*comment*/; }
      .escaped-size { container-type: s\69ze; }
      .reset { container: none; }
      .name-only { container: demo; }
      .commented-reset { container: none/*comment*/; }
      .commented-name-only { container: demo/*comment*/; }
      .commented-name-and-type { container: demo /*comment*/ / inline-size; }
    </style>
    <div id="invalid-style" class="base invalid-style"></div>
    <div id="invalid-none" class="base invalid-none"></div>
    <div id="commented-size" class="base commented-size"></div>
    <div id="escaped-size" class="base escaped-size"></div>
    <div id="reset" class="base reset"></div>
    <div id="name-only" class="base name-only"></div>
    <div id="commented-reset" class="base commented-reset"></div>
    <div id="commented-name-only" class="base commented-name-only"></div>
    <div id="commented-name-and-type" class="base commented-name-and-type"></div>
  "#;

  let styled = styled_tree_for(html);

  let invalid_style = find_by_id(&styled, "invalid-style").expect("invalid-style element");
  assert_eq!(invalid_style.styles.container_type, ContainerType::InlineSize);

  let invalid_none = find_by_id(&styled, "invalid-none").expect("invalid-none element");
  assert_eq!(invalid_none.styles.container_type, ContainerType::InlineSize);

  let commented_size = find_by_id(&styled, "commented-size").expect("commented-size element");
  assert_eq!(commented_size.styles.container_type, ContainerType::Size);

  let escaped_size = find_by_id(&styled, "escaped-size").expect("escaped-size element");
  assert_eq!(escaped_size.styles.container_type, ContainerType::Size);

  let reset = find_by_id(&styled, "reset").expect("reset element");
  assert_eq!(reset.styles.container_type, ContainerType::Normal);
  assert!(reset.styles.container_name.is_empty());

  let name_only = find_by_id(&styled, "name-only").expect("name-only element");
  assert_eq!(name_only.styles.container_type, ContainerType::Normal);
  assert_eq!(name_only.styles.container_name, vec!["demo".to_string()]);

  let commented_reset = find_by_id(&styled, "commented-reset").expect("commented-reset element");
  assert_eq!(commented_reset.styles.container_type, ContainerType::Normal);
  assert!(commented_reset.styles.container_name.is_empty());

  let commented_name_only =
    find_by_id(&styled, "commented-name-only").expect("commented-name-only element");
  assert_eq!(commented_name_only.styles.container_type, ContainerType::Normal);
  assert_eq!(
    commented_name_only.styles.container_name,
    vec!["demo".to_string()]
  );

  let commented_name_and_type =
    find_by_id(&styled, "commented-name-and-type").expect("commented-name-and-type element");
  assert_eq!(commented_name_and_type.styles.container_type, ContainerType::InlineSize);
  assert_eq!(
    commented_name_and_type.styles.container_name,
    vec!["demo".to_string()]
  );
}
