use crate::css::parser::{extract_scoped_css_sources, parse_stylesheet, rel_list_contains_stylesheet};
use crate::css::parser::{StylesheetLink, StylesheetSource};
use crate::dom::parse_html;
use crate::style::cascade::{apply_style_set_with_media_target_and_imports, StyledNode};
use crate::style::media::MediaContext;
use crate::style::style_set::StyleSet;
use crate::style::types::LineHeight;
use crate::style::values::LengthUnit;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

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

fn find_by_class<'a>(node: &'a StyledNode, class_name: &str) -> Option<&'a StyledNode> {
  if let Some(class) = node.node.get_attribute_ref("class") {
    if class
      .split_ascii_whitespace()
      .any(|name| name.eq_ignore_ascii_case(class_name))
    {
      return Some(node);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| find_by_class(child, class_name))
}

fn find_by_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .tag_name()
    .is_some_and(|name| name.eq_ignore_ascii_case(tag))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_tag(child, tag))
}

fn collect_stylesheet_source_css(source: &StylesheetSource, base_dir: &Path) -> Option<String> {
  match source {
    StylesheetSource::Inline(inline) => {
      if inline.disabled {
        return None;
      }
      Some(inline.css.clone())
    }
    StylesheetSource::External(StylesheetLink {
      href, rel, disabled, ..
    }) => {
      if *disabled {
        return None;
      }
      if !rel_list_contains_stylesheet(rel) {
        return None;
      }
      let href = href.trim();
      if href.is_empty()
        || href.starts_with("http:")
        || href.starts_with("https:")
        || href.starts_with("//")
        || href.starts_with('/')
      {
        return None;
      }
      fs::read_to_string(base_dir.join(href)).ok()
    }
  }
}

fn collect_stylesheet_sources_css(sources: &[StylesheetSource], base_dir: &Path) -> String {
  let mut css = String::new();
  for source in sources {
    if let Some(src) = collect_stylesheet_source_css(source, base_dir) {
      css.push_str(&src);
      css.push('\n');
    }
  }
  css
}

fn assert_line_height_px(line_height: &LineHeight, expected_px: f32) {
  match line_height {
    LineHeight::Length(len) => {
      assert_eq!(len.unit, LengthUnit::Px, "expected line-height to be px");
      assert!(
        (len.value - expected_px).abs() < 1e-6,
        "expected line-height {expected_px}px, got {}{}",
        len.value,
        format!("{:?}", len.unit).to_ascii_lowercase()
      );
    }
    other => panic!("expected line-height length, got {other:?}"),
  }
}

#[test]
fn font_shorthand_inherit_overrides_form_control_ua_typography() {
  // MDN (and other sites using Shadow DOM) often includes:
  //   button,input,select,textarea { font: inherit }
  // to ensure form controls match the surrounding site's typography.
  //
  // FastRender's UA stylesheet explicitly sets form-control fonts (user_agent.css:397+), so
  // `font: inherit` must correctly apply the CSS-wide keyword `inherit` to *all* font longhands.
  let html = r#"
    <!doctype html>
    <html>
      <body>
        <input id="text" type="text" value="X" />
        <button id="btn" type="button">Ok</button>
      </body>
    </html>
  "#;

  let css = r#"
    body {
      font-family: Inter;
      font-size: 20px;
      line-height: 30px;
    }
    input, button {
      /* Ensure the shorthand + CSS-wide keyword resets all font subproperties to inherited. */
      font: inherit;
    }
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet = parse_stylesheet(css).expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );

  let body = find_by_tag(&styled, "body").expect("body");
  let input = find_by_id(&styled, "text").expect("#text");
  let button = find_by_id(&styled, "btn").expect("#btn");

  // font-family should match the parent and not the UA default `sans-serif`.
  assert_eq!(
    input.styles.font_family.as_ref(),
    body.styles.font_family.as_ref(),
    "expected input font-family to inherit from body"
  );
  assert_eq!(
    button.styles.font_family.as_ref(),
    body.styles.font_family.as_ref(),
    "expected button font-family to inherit from body"
  );
  assert_eq!(
    input.styles.font_family.first().map(|s| s.as_str()),
    Some("Inter"),
    "expected input to use author font-family Inter"
  );

  // font-size should match the parent and not the UA default 13.3333px.
  assert!(
    (input.styles.font_size - 20.0).abs() < 1e-6,
    "expected input font-size 20px, got {}",
    input.styles.font_size
  );
  assert!(
    (button.styles.font_size - 20.0).abs() < 1e-6,
    "expected button font-size 20px, got {}",
    button.styles.font_size
  );

  // line-height is part of the `font` shorthand; `font: inherit` must inherit it too.
  assert_line_height_px(&input.styles.line_height, 30.0);
  assert_line_height_px(&button.styles.line_height, 30.0);
}

#[test]
fn mdn_fixture_form_controls_inherit_site_font_via_font_shorthand() {
  // Regression test for MDN's Shadow DOM styling pattern:
  //   button,input,select,textarea { font: inherit }
  //
  // The fixture includes form controls inside declarative shadow roots (language switcher and
  // sidebar filter input). These controls should use the MDN site font ("Inter"), rather than the
  // UA `sans-serif` fallback applied to form controls by default.
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture_dir = root.join(
    "tests/pages/fixtures/developer.mozilla.org_en-US_docs_Learn_Forms_Your_first_form",
  );
  let html = fs::read_to_string(fixture_dir.join("index.html")).expect("fixture html");
  let dom = parse_html(&html).expect("parsed fixture html");

  let sources = extract_scoped_css_sources(&dom);
  let document_css = collect_stylesheet_sources_css(&sources.document, &fixture_dir);
  let document_sheet = parse_stylesheet(&document_css).expect("parse document stylesheet");

  let mut shadows = HashMap::new();
  for (host, sources) in &sources.shadows {
    let css = collect_stylesheet_sources_css(sources, &fixture_dir);
    if css.trim().is_empty() {
      continue;
    }
    let sheet = parse_stylesheet(&css).expect("parse shadow stylesheet");
    shadows.insert(*host, sheet);
  }

  let style_set = StyleSet {
    document: document_sheet,
    shadows,
  };
  let media = MediaContext::screen(1200.0, 800.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );

  let sidebar_filter_host =
    find_by_tag(&styled, "mdn-sidebar-filter").expect("<mdn-sidebar-filter>");
  let sidebar_filter_input = find_by_id(&styled, "input").expect("sidebar filter #input");

  let language_switcher_host =
    find_by_tag(&styled, "mdn-language-switcher").expect("<mdn-language-switcher>");
  let language_switcher_button =
    find_by_class(&styled, "language-switcher__button").expect(".language-switcher__button");

  // Ensure the global MDN font is set.
  assert_eq!(
    sidebar_filter_host.styles.font_family.first().map(|s| s.as_str()),
    Some("Inter"),
    "expected MDN fixture host font-family to start with Inter"
  );

  // `font: inherit` must override the UA `sans-serif` font for form controls inside shadow roots.
  assert_eq!(
    sidebar_filter_input.styles.font_family.as_ref(),
    sidebar_filter_host.styles.font_family.as_ref(),
    "expected sidebar filter input font-family to inherit from host"
  );
  assert_eq!(
    language_switcher_button.styles.font_family.as_ref(),
    language_switcher_host.styles.font_family.as_ref(),
    "expected language switcher button font-family to inherit from host"
  );

  // Font-size should inherit (host uses MDN's root font-size, not UA form-control 13.3333px).
  assert!(
    (sidebar_filter_input.styles.font_size - sidebar_filter_host.styles.font_size).abs() < 1e-6,
    "expected sidebar filter input font-size to inherit from host (host={}, input={})",
    sidebar_filter_host.styles.font_size,
    sidebar_filter_input.styles.font_size
  );
  assert!(
    (language_switcher_button.styles.font_size - language_switcher_host.styles.font_size).abs() < 1e-6,
    "expected language switcher button font-size to inherit from host (host={}, button={})",
    language_switcher_host.styles.font_size,
    language_switcher_button.styles.font_size
  );
  assert!(
    (sidebar_filter_input.styles.font_size - 13.333333).abs() > 1e-3,
    "expected sidebar filter input font-size to override UA 13.3333px, got {}",
    sidebar_filter_input.styles.font_size
  );

  // line-height is part of the font shorthand; ensure it inherits too.
  assert_eq!(
    sidebar_filter_input.styles.line_height,
    sidebar_filter_host.styles.line_height,
    "expected sidebar filter input line-height to inherit from host"
  );
  assert_eq!(
    language_switcher_button.styles.line_height,
    language_switcher_host.styles.line_height,
    "expected language switcher button line-height to inherit from host"
  );
}

