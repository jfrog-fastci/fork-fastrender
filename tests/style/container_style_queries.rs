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
fn container_style_query_resolves_var_in_custom_property_value() {
  let html = r#"
    <style>
      .match { container-type: inline-size; --foo: bar; --bar: bar; }
      .miss { container-type: inline-size; --foo: bar; --bar: baz; }
      .child { color: rgb(0 0 255); }
      @container style(--foo: var(--bar)) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div class="match">
      <div id="match" class="child">hello</div>
    </div>
    <div class="miss">
      <div id="miss" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let matched = find_by_id(&styled, "match").expect("match element");
  let missed = find_by_id(&styled, "miss").expect("miss element");
  assert_eq!(matched.styles.color, Rgba::rgb(255, 0, 0));
  assert_eq!(missed.styles.color, Rgba::rgb(0, 0, 255));
}

#[test]
fn container_style_query_resolves_var_in_property_value() {
  let html = r#"
    <style>
      .container { container-type: inline-size; color: rgb(255 0 0); --c: rgb(255 0 0); }
      .child { color: rgb(0 0 255); }
      @container style(color: var(--c)) {
        .child { color: rgb(1 2 3); }
      }
    </style>
    <div class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(1, 2, 3));
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
fn container_style_query_matches_computed_color() {
  let html = r#"
    <style>
      .container { container-type: inline-size; color: rgb(255 0 0); }
      .child { color: rgb(0 0 255); }
      @container style(color: rgb(255 0 0)) {
        .child { color: rgb(5 6 7); }
      }
    </style>
    <div class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(5, 6, 7));
}

#[test]
fn container_style_query_boolean_feature_matches_when_non_initial() {
  let html = r#"
    <style>
      .container-inline { container-type: inline-size; display: inline; }
      .container-block { container-type: inline-size; display: block; }
      .child { color: rgb(0 0 255); }
      @container style(display) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div class="container-inline">
      <div id="inline" class="child">hello</div>
    </div>
    <div class="container-block">
      <div id="block" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let inline = find_by_id(&styled, "inline").expect("inline element");
  let block = find_by_id(&styled, "block").expect("block element");
  assert_eq!(inline.styles.color, Rgba::rgb(0, 0, 255));
  assert_eq!(block.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_range_feature_matches() {
  let html = r#"
    <style>
      .container-small { container-type: inline-size; font-size: 12px; }
      .container-large { container-type: inline-size; font-size: 16px; }
      .child { color: rgb(0 0 255); }
      @container style(font-size > 12px) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div class="container-small">
      <div id="small" class="child">hello</div>
    </div>
    <div class="container-large">
      <div id="large" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let small = find_by_id(&styled, "small").expect("small element");
  let large = find_by_id(&styled, "large").expect("large element");
  assert_eq!(small.styles.color, Rgba::rgb(0, 0, 255));
  assert_eq!(large.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_range_feature_resolves_var_value() {
  let html = r#"
    <style>
      .container { container-type: inline-size; font-size: 16px; --min: 12px; }
      .child { color: rgb(0 0 255); }
      @container style(font-size > var(--min)) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div class="container">
      <div id="target" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_boolean_custom_property_respects_property_initial_value() {
  let html = r#"
    <style>
      @property --x {
        syntax: "<length>";
        inherits: false;
        initial-value: 10px;
      }
      .container { container-type: inline-size; }
      .container-set { container-type: inline-size; --x: 11px; }
      .child { color: rgb(0 0 255); }
      @container style(--x) {
        .child { color: rgb(255 0 0); }
      }
    </style>
    <div class="container">
      <div id="initial" class="child">hello</div>
    </div>
    <div class="container-set">
      <div id="set" class="child">hello</div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let initial = find_by_id(&styled, "initial").expect("initial element");
  let set = find_by_id(&styled, "set").expect("set element");
  assert_eq!(initial.styles.color, Rgba::rgb(0, 0, 255));
  assert_eq!(set.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn container_style_query_supports_logical_operators() {
  let html = r#"
    <style>
      .c1 { container-type: inline-size; --foo: bar; color: rgb(255 0 0); }
      .c2 { container-type: inline-size; --foo: baz; color: rgb(0 0 0); }
      .child { color: rgb(0 0 255); }

      @container style((--foo: bar) and (color: rgb(255 0 0))) {
        .and { color: rgb(1 2 3); }
      }
      @container style((--foo: bar) or (--foo: baz)) {
        .or { color: rgb(4 5 6); }
      }
      @container style(not (--foo: bar)) {
        .not { color: rgb(7 8 9); }
      }
    </style>
    <div class="c1">
      <div id="and1" class="child and">and</div>
      <div id="or1" class="child or">or</div>
      <div id="not1" class="child not">not</div>
    </div>
    <div class="c2">
      <div id="and2" class="child and">and</div>
      <div id="or2" class="child or">or</div>
      <div id="not2" class="child not">not</div>
    </div>
  "#;

  let styled = styled_tree_for(html);

  let and1 = find_by_id(&styled, "and1").expect("and1 element");
  let and2 = find_by_id(&styled, "and2").expect("and2 element");
  let or1 = find_by_id(&styled, "or1").expect("or1 element");
  let or2 = find_by_id(&styled, "or2").expect("or2 element");
  let not1 = find_by_id(&styled, "not1").expect("not1 element");
  let not2 = find_by_id(&styled, "not2").expect("not2 element");

  assert_eq!(and1.styles.color, Rgba::rgb(1, 2, 3));
  assert_eq!(and2.styles.color, Rgba::rgb(0, 0, 255));

  assert_eq!(or1.styles.color, Rgba::rgb(4, 5, 6));
  assert_eq!(or2.styles.color, Rgba::rgb(4, 5, 6));

  assert_eq!(not1.styles.color, Rgba::rgb(0, 0, 255));
  assert_eq!(not2.styles.color, Rgba::rgb(7, 8, 9));
}

#[test]
fn container_type_rejects_none_and_style_keywords_and_container_shorthand_resets_to_normal() {
  let html = r#"
    <style>
      .base { container-type: inline-size; }
      .invalid-style { container-type: style; }
      .invalid-none { container-type: none; }
      .scroll-state { container-type: scroll-state; }
      .scroll-state-and-size { container-type: scroll-state size; }
      .commented-size { container-type: size/*comment*/; }
      .escaped-size { container-type: s\69ze; }
      .reset { container: none; }
      .name-only { container: demo; }
      .shorthand-scroll-state { container: demo / scroll-state; }
      .shorthand-scroll-state-and-size { container: demo / scroll-state size; }
      .commented-reset { container: none/*comment*/; }
      .commented-name-only { container: demo/*comment*/; }
      .commented-name-and-type { container: demo /*comment*/ / inline-size; }
    </style>
    <div id="invalid-style" class="base invalid-style"></div>
    <div id="invalid-none" class="base invalid-none"></div>
    <div id="scroll-state" class="base scroll-state"></div>
    <div id="scroll-state-and-size" class="base scroll-state-and-size"></div>
    <div id="commented-size" class="base commented-size"></div>
    <div id="escaped-size" class="base escaped-size"></div>
    <div id="reset" class="base reset"></div>
    <div id="name-only" class="base name-only"></div>
    <div id="shorthand-scroll-state" class="base shorthand-scroll-state"></div>
    <div id="shorthand-scroll-state-and-size" class="base shorthand-scroll-state-and-size"></div>
    <div id="commented-reset" class="base commented-reset"></div>
    <div id="commented-name-only" class="base commented-name-only"></div>
    <div id="commented-name-and-type" class="base commented-name-and-type"></div>
  "#;

  let styled = styled_tree_for(html);

  let invalid_style = find_by_id(&styled, "invalid-style").expect("invalid-style element");
  assert_eq!(invalid_style.styles.container_type, ContainerType::InlineSize);

  let invalid_none = find_by_id(&styled, "invalid-none").expect("invalid-none element");
  assert_eq!(invalid_none.styles.container_type, ContainerType::InlineSize);

  let scroll_state = find_by_id(&styled, "scroll-state").expect("scroll-state element");
  assert_eq!(scroll_state.styles.container_type, ContainerType::Normal);

  let scroll_state_and_size =
    find_by_id(&styled, "scroll-state-and-size").expect("scroll-state-and-size element");
  assert_eq!(scroll_state_and_size.styles.container_type, ContainerType::Size);

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

  let shorthand_scroll_state =
    find_by_id(&styled, "shorthand-scroll-state").expect("shorthand-scroll-state element");
  assert_eq!(shorthand_scroll_state.styles.container_type, ContainerType::Normal);
  assert_eq!(
    shorthand_scroll_state.styles.container_name,
    vec!["demo".to_string()]
  );

  let shorthand_scroll_state_and_size = find_by_id(&styled, "shorthand-scroll-state-and-size")
    .expect("shorthand-scroll-state-and-size element");
  assert_eq!(
    shorthand_scroll_state_and_size.styles.container_type,
    ContainerType::Size
  );
  assert_eq!(
    shorthand_scroll_state_and_size.styles.container_name,
    vec!["demo".to_string()]
  );

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

#[test]
fn container_global_keywords_apply_to_container_type_and_shorthand() {
  let html = r#"
    <style>
      #parent { container-type: inline-size; container-name: demo; }
      #type-inherit { container-type: inherit; }
      #type-initial { container-type: inline-size; container-type: initial; }
      #shorthand-inherit { container: inherit; }
      #shorthand-initial { container: initial; }
    </style>
    <div id="parent">
      <div id="type-inherit"></div>
      <div id="type-initial"></div>
      <div id="shorthand-inherit"></div>
      <div id="shorthand-initial"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);

  let type_inherit = find_by_id(&styled, "type-inherit").expect("type-inherit element");
  assert_eq!(type_inherit.styles.container_type, ContainerType::InlineSize);
  assert!(type_inherit.styles.container_name.is_empty());

  let type_initial = find_by_id(&styled, "type-initial").expect("type-initial element");
  assert_eq!(type_initial.styles.container_type, ContainerType::Normal);

  let shorthand_inherit =
    find_by_id(&styled, "shorthand-inherit").expect("shorthand-inherit element");
  assert_eq!(shorthand_inherit.styles.container_type, ContainerType::InlineSize);
  assert_eq!(
    shorthand_inherit.styles.container_name,
    vec!["demo".to_string()]
  );

  let shorthand_initial =
    find_by_id(&styled, "shorthand-initial").expect("shorthand-initial element");
  assert_eq!(shorthand_initial.styles.container_type, ContainerType::Normal);
  assert!(shorthand_initial.styles.container_name.is_empty());
}
