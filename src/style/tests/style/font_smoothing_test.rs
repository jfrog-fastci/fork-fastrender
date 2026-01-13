use crate::css::parser::parse_stylesheet;
use crate::css::supports::supports_declaration;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::FontSmoothing;

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

fn styled_root(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0))
}

#[test]
fn webkit_font_smoothing_antialiased_is_parsed_and_inherited() {
  let styled =
    styled_root(r#"<div style="-webkit-font-smoothing: antialiased"><span></span></div>"#);
  let div = find_first(&styled, "div").expect("div");
  let span = find_first(div, "span").expect("span");
  assert_eq!(div.styles.font_smoothing, FontSmoothing::Grayscale);
  assert_eq!(span.styles.font_smoothing, FontSmoothing::Grayscale);
}

#[test]
fn moz_osx_font_smoothing_grayscale_is_parsed() {
  let styled =
    styled_root(r#"<div style="-moz-osx-font-smoothing: grayscale"><span></span></div>"#);
  let div = find_first(&styled, "div").expect("div");
  assert_eq!(div.styles.font_smoothing, FontSmoothing::Grayscale);
}

#[test]
fn font_smooth_never_is_parsed_and_inherited() {
  let styled = styled_root(r#"<div style="font-smooth: never"><span></span></div>"#);
  let div = find_first(&styled, "div").expect("div");
  let span = find_first(div, "span").expect("span");
  assert_eq!(div.styles.font_smoothing, FontSmoothing::None);
  assert_eq!(span.styles.font_smoothing, FontSmoothing::None);
}

#[test]
fn font_smooth_always_is_parsed_and_inherited() {
  let styled = styled_root(r#"<div style="font-smooth: always"><span></span></div>"#);
  let div = find_first(&styled, "div").expect("div");
  let span = find_first(div, "span").expect("span");
  assert_eq!(div.styles.font_smoothing, FontSmoothing::Grayscale);
  assert_eq!(span.styles.font_smoothing, FontSmoothing::Grayscale);
}

#[test]
fn supports_font_smoothing_declarations() {
  assert!(supports_declaration(
    "-webkit-font-smoothing",
    "antialiased"
  ));
  assert!(supports_declaration(
    "-webkit-font-smoothing",
    "subpixel-antialiased"
  ));
  assert!(!supports_declaration("-webkit-font-smoothing", "bogus"));

  assert!(supports_declaration("-moz-osx-font-smoothing", "grayscale"));
  assert!(!supports_declaration(
    "-moz-osx-font-smoothing",
    "subpixel-antialiased"
  ));

  assert!(supports_declaration("font-smooth", "never"));
  assert!(supports_declaration("font-smooth", "always"));
  assert!(!supports_declaration("font-smooth", "auto auto"));
}
