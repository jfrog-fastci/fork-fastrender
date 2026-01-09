use fastrender::api::FastRender;
use fastrender::api::FastRenderConfig;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::dom::DomNodeType;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaType;
use fastrender::style::types::ColorSchemeEntry;
use fastrender::style::types::ColorSchemePreference;
use std::collections::HashMap;

fn styled_tree_for(html: &str, apply_meta_color_scheme: bool) -> StyledNode {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PREFERS_COLOR_SCHEME".to_string(),
    "dark".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_meta_color_scheme(apply_meta_color_scheme)
    .with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
    .styled_tree
}

fn find_element<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let DomNodeType::Element { tag_name, .. } = &node.node.node_type {
    if tag_name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  node.children.iter().find_map(|child| find_element(child, tag))
}

#[test]
fn meta_color_scheme_is_ignored_by_default() {
  let html = r#"
    <html>
      <head>
        <meta name="color-scheme" content="light dark">
        <style>
          html { color: light-dark(rgb(255 0 0), rgb(0 0 255)); }
        </style>
      </head>
      <body></body>
    </html>
  "#;
  let styled = styled_tree_for(html, false);
  let html_node = find_element(&styled, "html").expect("html element");
  assert_eq!(html_node.styles.color_scheme, ColorSchemePreference::Normal);
  assert!(!html_node.styles.used_dark_color_scheme);
  assert_eq!(html_node.styles.color, Rgba::rgb(255, 0, 0));
}

#[test]
fn meta_color_scheme_applies_as_ua_root_baseline_when_enabled() {
  let html = r#"
    <html>
      <head>
        <meta name="color-scheme" content="light dark">
        <style>
          html { color: light-dark(rgb(255 0 0), rgb(0 0 255)); }
        </style>
      </head>
      <body></body>
    </html>
  "#;
  let styled = styled_tree_for(html, true);
  let html_node = find_element(&styled, "html").expect("html element");
  assert_eq!(
    html_node.styles.color_scheme,
    ColorSchemePreference::Supported {
      schemes: vec![ColorSchemeEntry::Light, ColorSchemeEntry::Dark],
      only: false,
    }
  );
  assert!(html_node.styles.used_dark_color_scheme);
  assert_eq!(html_node.styles.color, Rgba::rgb(0, 0, 255));
}

#[test]
fn author_color_scheme_overrides_meta_color_scheme() {
  let html = r#"
    <html>
      <head>
        <meta name="color-scheme" content="light dark">
        <style>
          html {
            color-scheme: light;
            color: light-dark(rgb(255 0 0), rgb(0 0 255));
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;
  let styled = styled_tree_for(html, true);
  let html_node = find_element(&styled, "html").expect("html element");
  assert_eq!(
    html_node.styles.color_scheme,
    ColorSchemePreference::Supported {
      schemes: vec![ColorSchemeEntry::Light],
      only: false,
    }
  );
  assert!(!html_node.styles.used_dark_color_scheme);
  assert_eq!(html_node.styles.color, Rgba::rgb(255, 0, 0));
}

