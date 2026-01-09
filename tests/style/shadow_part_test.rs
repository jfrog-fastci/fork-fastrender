use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::StyleSheet;
use fastrender::dom::{enumerate_dom_ids, parse_html, DomNode};
use fastrender::style::cascade::{
  apply_style_set_with_media_target_and_imports, apply_styles_with_media_target_and_imports,
  ContainerQueryContext, ContainerQueryInfo, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use fastrender::style::types::ContainerType;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::Rgba;
use std::collections::HashMap;
use std::sync::Arc;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_dom_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn part_selector_styles_shadow_content() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <span id="label" part="label">Hello</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet =
    parse_stylesheet("x-host::part(label) { color: rgb(1, 2, 3); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "label").expect("shadow element");

  assert_eq!(label.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn part_selector_with_multiple_names_matches_intersection() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <span id="both" part="name badge">Hello</span>
        <span id="name-only" part="name">Hello</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let empty_style_set = StyleSet {
    document: StyleSheet::new(),
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let baseline = apply_style_set_with_media_target_and_imports(
    &dom,
    &empty_style_set,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let baseline_name_only = find_by_id(&baseline, "name-only").expect("shadow element");
  let baseline_name_only_color = baseline_name_only.styles.color;

  let stylesheet =
    parse_stylesheet("x-host::part(name badge) { color: rgb(4, 5, 6); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let both = find_by_id(&styled, "both").expect("shadow element");
  let name_only = find_by_id(&styled, "name-only").expect("shadow element");

  assert_eq!(both.styles.color, Rgba::rgb(4, 5, 6));
  assert_eq!(name_only.styles.color, baseline_name_only_color);
  assert_ne!(name_only.styles.color, Rgba::rgb(4, 5, 6));
}

#[test]
fn light_dom_selector_does_not_cross_shadow_boundary() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <span id="shadow-label" class="label" part="label">Shadow</span>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let empty_style_set = StyleSet {
    document: StyleSheet::new(),
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let baseline = apply_style_set_with_media_target_and_imports(
    &dom,
    &empty_style_set,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let baseline_color = find_by_id(&baseline, "shadow-label")
    .expect("shadow element")
    .styles
    .color;

  let stylesheet = parse_stylesheet(".label { color: rgb(9, 8, 7); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "shadow-label").expect("shadow element");

  assert_eq!(label.styles.color, baseline_color);
}

#[test]
fn exportparts_chain_maps_part_names() {
  let html = r#"
    <x-outer id="outer">
      <template shadowroot="open">
        <x-inner id="inner" exportparts="label:outer-label">
          <template shadowroot="open">
            <span id="inner-label" part="label">Inner</span>
          </template>
        </x-inner>
      </template>
    </x-outer>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet =
    parse_stylesheet("x-outer::part(outer-label) { color: rgb(10, 20, 30); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "inner-label").expect("inner part");

  assert_eq!(label.styles.color, Rgba::rgb(10, 20, 30));
}

#[test]
fn nested_shadow_roots_resolve_part_order_stably() {
  let html = r#"
    <x-outer id="outer">
      <template shadowroot="open">
        <style>
          .inner::part(label) {
            color: rgb(1, 2, 3);
            border-top: 7px solid rgb(1, 2, 3);
          }
        </style>
        <x-inner id="inner" class="inner">
          <template shadowroot="open">
            <style>
              .target {
                color: rgb(4, 5, 6);
              }
            </style>
            <span id="part" class="target" part="label">Inner</span>
          </template>
        </x-inner>
      </template>
    </x-outer>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media_target_and_imports(
    &dom,
    &StyleSheet::new(),
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let part = find_by_id(&styled, "part").expect("shadow part");

  assert_eq!(part.styles.border_top_width, Length::px(7.0));
  assert_eq!(part.styles.color, Rgba::rgb(4, 5, 6));
}

#[test]
fn exportparts_applies_renamed_parts_at_boundary() {
  let html = r#"
    <x-host id="host" exportparts="label:outer-label">
      <template shadowroot="open">
        <span id="inner" part="label">Inner</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet =
    parse_stylesheet("x-host::part(outer-label) { color: rgb(10, 20, 30); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let inner = find_by_id(&styled, "inner").expect("exported part");

  assert_eq!(inner.styles.color, Rgba::rgb(10, 20, 30));
}

#[test]
fn exportparts_renaming_hides_original_name_in_containing_scope() {
  let html = r#"
    <x-host id="host" exportparts="label:outer-label">
      <template shadowroot="open">
        <span id="inner" part="label">Inner</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  // Only the exported alias should be visible from the document; the original name
  // stays within the shadow boundary.
  let baseline_style_set = StyleSet {
    document: StyleSheet::new(),
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let baseline = apply_style_set_with_media_target_and_imports(
    &dom,
    &baseline_style_set,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let baseline_color = find_by_id(&baseline, "inner")
    .expect("exported part")
    .styles
    .color;

  let stylesheet =
    parse_stylesheet("x-host::part(label) { color: rgb(200, 10, 20); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let inner = find_by_id(&styled, "inner").expect("exported part");

  assert_eq!(inner.styles.color, baseline_color);
  assert_ne!(inner.styles.color, Rgba::rgb(200, 10, 20));
}

#[test]
fn exportparts_empty_exports_no_parts() {
  let html = r#"
    <x-host id="host" exportparts="">
      <template shadowroot="open">
        <span id="inner" part="label">Inner</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let media = MediaContext::screen(800.0, 600.0);

  let baseline_style_set = StyleSet {
    document: StyleSheet::new(),
    shadows: HashMap::new(),
  };
  let baseline = apply_style_set_with_media_target_and_imports(
    &dom,
    &baseline_style_set,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let baseline_color = find_by_id(&baseline, "inner")
    .expect("shadow part")
    .styles
    .color;

  let stylesheet =
    parse_stylesheet("x-host::part(label) { color: rgb(200, 10, 20); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let inner = find_by_id(&styled, "inner").expect("shadow part");

  assert_eq!(inner.styles.color, baseline_color);
  assert_ne!(inner.styles.color, Rgba::rgb(200, 10, 20));
}

#[test]
fn document_host_part_selector_does_not_apply() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <span id="label" part="label">Hello</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let media = MediaContext::screen(800.0, 600.0);

  let empty_style_set = StyleSet {
    document: StyleSheet::new(),
    shadows: HashMap::new(),
  };
  let baseline = apply_style_set_with_media_target_and_imports(
    &dom,
    &empty_style_set,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let baseline_color = find_by_id(&baseline, "label")
    .expect("shadow element")
    .styles
    .color;

  let stylesheet =
    parse_stylesheet(":host::part(label) { color: rgb(1, 2, 3); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "label").expect("shadow element");

  assert_eq!(
    label.styles.color, baseline_color,
    "document-scoped :host must not style parts inside a shadow tree"
  );
  assert_ne!(label.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn part_container_query_is_evaluated_against_the_part_element() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <style>
          #container { container-type: inline-size; }
        </style>
        <div id="container">
          <span id="part" part="foo">Hello</span>
        </div>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let container = find_dom_by_id(&dom, "container").expect("container element");
  let container_id = *ids
    .get(&(container as *const DomNode))
    .expect("container node id");

  let stylesheet = parse_stylesheet(
    r#"
      x-host::part(foo) { color: rgb(0, 0, 255); }
      @container (min-width: 150px) {
        x-host::part(foo) { color: rgb(255, 0, 0); }
      }
    "#,
  )
  .expect("stylesheet");
  let media = MediaContext::screen(800.0, 600.0);

  let cascade = |inline_size: f32| {
    let ctx = ContainerQueryContext {
      base_media: media.clone(),
      containers: HashMap::from([(
        container_id,
        ContainerQueryInfo {
          width: inline_size,
          height: 300.0,
          inline_size,
          block_size: 300.0,
          container_type: ContainerType::InlineSize,
          names: Vec::new(),
          font_size: 16.0,
          styles: Arc::new(ComputedStyle::default()),
        },
      )]),
    };

    apply_styles_with_media_target_and_imports(
      &dom,
      &stylesheet,
      &media,
      None,
      None,
      None,
      Some(&ctx),
      None,
      None,
    )
  };

  let styled_small = cascade(100.0);
  let part_small = find_by_id(&styled_small, "part").expect("part element");
  assert_eq!(part_small.styles.color, Rgba::rgb(0, 0, 255));

  let styled_large = cascade(200.0);
  let part_large = find_by_id(&styled_large, "part").expect("part element");
  assert_eq!(part_large.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn part_container_query_uses_flat_tree_ancestors_when_host_is_slotted() {
  let html = r#"
    <x-outer id="outer">
      <template shadowroot="open">
        <style>
          #container { container-type: inline-size; }
        </style>
        <div id="container">
          <slot></slot>
        </div>
      </template>
      <x-inner id="inner">
        <template shadowroot="open">
          <span id="part" part="foo">Hello</span>
        </template>
      </x-inner>
    </x-outer>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let ids = enumerate_dom_ids(&dom);
  let container = find_dom_by_id(&dom, "container").expect("container element");
  let container_id = *ids
    .get(&(container as *const DomNode))
    .expect("container node id");

  let stylesheet = parse_stylesheet(
    r#"
      x-inner::part(foo) { color: rgb(0, 0, 255); }
      @container (min-width: 150px) {
        x-inner::part(foo) { color: rgb(255, 0, 0); }
      }
    "#,
  )
  .expect("stylesheet");
  let media = MediaContext::screen(800.0, 600.0);

  let cascade = |inline_size: f32| {
    let mut style = ComputedStyle::default();
    style.container_type = ContainerType::InlineSize;
    let style = Arc::new(style);
    let block_size = 300.0;
    let ctx = ContainerQueryContext {
      base_media: media.clone(),
      containers: HashMap::from([(
        container_id,
        ContainerQueryInfo {
          width: inline_size,
          height: block_size,
          inline_size,
          block_size,
          container_type: ContainerType::InlineSize,
          names: Vec::new(),
          font_size: style.font_size,
          styles: Arc::clone(&style),
        },
      )]),
    };

    apply_styles_with_media_target_and_imports(
      &dom,
      &stylesheet,
      &media,
      None,
      None,
      None,
      Some(&ctx),
      None,
      None,
    )
  };

  let styled_small = cascade(100.0);
  let part_small = find_by_id(&styled_small, "part").expect("part element");
  assert_eq!(part_small.styles.color, Rgba::rgb(0, 0, 255));

  let styled_large = cascade(200.0);
  let part_large = find_by_id(&styled_large, "part").expect("part element");
  assert_eq!(part_large.styles.color, Rgba::rgb(255, 0, 0));
}
