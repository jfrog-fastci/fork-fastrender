use crate::css::parser::parse_stylesheet;
use crate::css::types::CssImportLoader;
use crate::dom;
use crate::style::cascade::{apply_styles_with_media, StyledNode};
use crate::style::display::Display;
use crate::style::media::MediaContext;
use std::collections::HashMap;
use std::io;

const BASE_URL: &str = "https://example.com/main.css";

struct MapImportLoader {
  styles: HashMap<String, String>,
}

impl MapImportLoader {
  fn new() -> Self {
    Self {
      styles: HashMap::new(),
    }
  }

  fn with(mut self, url: &str, css: &str) -> Self {
    self.styles.insert(url.to_string(), css.to_string());
    self
  }
}

impl CssImportLoader for MapImportLoader {
  fn load(&self, url: &str) -> crate::error::Result<String> {
    self
      .styles
      .get(url)
      .cloned()
      .ok_or(crate::error::Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing import {url}"),
      )))
  }
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

#[test]
fn supports_selector_evaluates_namespace_prefixes_using_stylesheet_namespaces() {
  let dom = dom::parse_html(r#"<svg><rect id="r"></rect></svg>"#).unwrap();
  let css = r#"
    @namespace svg "http://www.w3.org/2000/svg";

    svg|rect { display: block; }

    @supports selector(svg|rect) {
      svg|rect { display: none; }
    }
  "#;

  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let rect = find_by_id(&styled, "r").expect("rect");
  assert_eq!(rect.styles.display, Display::None);
}

#[test]
fn supports_selector_in_imported_stylesheets_remains_correct_after_import_inlining() {
  let imported_css = r#"
    @namespace svg "http://www.w3.org/2000/svg";

    svg|rect { display: block; }

    @supports selector(svg|rect) {
      svg|rect { display: none; }
    }
  "#;
  let loader = MapImportLoader::new().with("https://example.com/a.css", imported_css);
  let media = MediaContext::screen(800.0, 600.0);
  let stylesheet = parse_stylesheet(r#"@import "a.css";"#).unwrap();
  let resolved = stylesheet
    .resolve_imports(&loader, Some(BASE_URL), &media)
    .unwrap();

  let dom = dom::parse_html(r#"<svg><rect id="r"></rect></svg>"#).unwrap();
  let styled = apply_styles_with_media(&dom, &resolved, &media);
  let rect = find_by_id(&styled, "r").expect("rect");
  assert_eq!(rect.styles.display, Display::None);
}
