use fastrender::css::parser::{extract_scoped_css_sources, parse_stylesheet, StylesheetSource};
use fastrender::css::types::StyleSheet;
use fastrender::dom::parse_html;
use fastrender::style::cascade::{apply_style_set_with_media_target_and_imports, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use fastrender::Rgba;
use std::collections::HashMap;

fn stylesheet_from_sources(sources: &[StylesheetSource]) -> StyleSheet {
  let mut combined = Vec::new();
  for source in sources {
    let StylesheetSource::Inline(inline) = source else {
      continue;
    };
    if inline.disabled || inline.css.trim().is_empty() {
      continue;
    }
    if let Ok(sheet) = parse_stylesheet(&inline.css) {
      combined.extend(sheet.rules);
    }
  }
  StyleSheet { rules: combined }
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn closed_shadow_root_stylesheet_applies() {
  let html = r#"
    <div id="host">
      <template shadowroot="closed">
        <style>
          #inner { color: rgb(1, 2, 3); }
        </style>
        <span id="inner">x</span>
      </template>
    </div>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = HashMap::new();
  for (host, sources) in scoped_sources.shadows {
    shadows.insert(host, stylesheet_from_sources(&sources));
  }
  let style_set = StyleSet {
    document: stylesheet_from_sources(&scoped_sources.document),
    shadows,
  };

  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let inner = find_by_id(&styled, "inner").expect("shadow element");

  assert_eq!(inner.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn document_rules_do_not_pierce_closed_shadow_root() {
  let html = r#"
    <div id="host">
      <template shadowroot="closed">
        <span id="inner">x</span>
      </template>
    </div>
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
    .expect("shadow element")
    .styles
    .color;

  let stylesheet = parse_stylesheet("span { color: rgb(9, 9, 9); }").expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let inner = find_by_id(&styled, "inner").expect("shadow element");

  assert_eq!(inner.styles.color, baseline_color);
  assert_ne!(inner.styles.color, Rgba::rgb(9, 9, 9));
}

#[test]
fn part_selector_matches_into_closed_shadow_root() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="closed">
        <span id="label" part="label">Hello</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet = parse_stylesheet("x-host::part(label) { color: rgb(4, 5, 6); }")
    .expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "label").expect("shadow element");

  assert_eq!(label.styles.color, Rgba::rgb(4, 5, 6));
}

#[test]
fn exportparts_chain_works_across_closed_shadow_root() {
  let html = r#"
    <x-host id="host" exportparts="label:outer">
      <template shadowroot="closed">
        <span id="label" part="label">Hello</span>
      </template>
    </x-host>
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet = parse_stylesheet("x-host::part(outer) { color: rgb(7, 8, 9); }")
    .expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );
  let label = find_by_id(&styled, "label").expect("exported part");

  assert_eq!(label.styles.color, Rgba::rgb(7, 8, 9));
}

