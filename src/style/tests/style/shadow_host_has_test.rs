use fastrender::css::parser::{extract_scoped_css_sources, parse_stylesheet, StylesheetSource};
use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_style_set_with_media_target_and_imports, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;

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
  StyleSheet {
    namespaces: Default::default(),
    rules: combined,
  }
}

fn find_styled_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_styled_by_id(child, id))
}

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

#[test]
fn host_has_sees_shadow_tree_descendants_but_not_light_dom() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          :host { display: block; }
          :host:has(.inner) { display: inline; }
          :host:has(.light) { display: inline-block; }
        </style>
        <div class="inner"></div>
      </template>
      <div class="light"></div>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = std::collections::HashMap::new();
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

  assert_eq!(
    display(find_styled_by_id(&styled, "host").expect("host")),
    "inline"
  );
}

#[test]
fn document_has_does_not_pierce_shadow_tree() {
  let html = r#"
    <style>
      #host { display: block; }
      #host:has(.inner) { display: inline; }
    </style>
    <div id="host">
      <template shadowroot="open">
        <div class="inner"></div>
      </template>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = std::collections::HashMap::new();
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

  assert_eq!(
    display(find_styled_by_id(&styled, "host").expect("host")),
    "block"
  );
}

#[test]
fn host_has_does_not_match_slotted_light_dom() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          :host { display: block; }
          :host:has(.slotted) { display: inline; }
        </style>
        <slot></slot>
      </template>
      <span class="slotted">Light</span>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = std::collections::HashMap::new();
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

  assert_eq!(
    display(find_styled_by_id(&styled, "host").expect("host")),
    "block"
  );
}

#[test]
fn shadow_host_is_featureless_for_subject_matching() {
  let html = r#"
    <div id="host" class="foo">
      <template shadowroot="open">
        <style>
          :host { display: block; }
          :host(.foo) { display: inline; }
          .foo:host { display: inline-block; }
        </style>
      </template>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = std::collections::HashMap::new();
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

  assert_eq!(
    display(find_styled_by_id(&styled, "host").expect("host")),
    "inline"
  );
}

#[test]
fn featureless_host_does_not_unlock_has_inside_is() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          :host { display: block; }
          :host:has(.inner) { display: inline; }
          :host:is(:has(.inner)) { display: inline-block; }
        </style>
        <div class="inner"></div>
      </template>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let scoped_sources = extract_scoped_css_sources(&dom);
  let mut shadows = std::collections::HashMap::new();
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

  assert_eq!(
    display(find_styled_by_id(&styled, "host").expect("host")),
    "inline"
  );
}
