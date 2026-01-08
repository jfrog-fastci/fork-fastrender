use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::FontStyle;

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

fn styled_div(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, "div").expect("div").clone()
}

#[test]
fn font_style_oblique_does_not_use_substring_matching() {
  // `oblique10deg` is a single identifier token, so it must not be interpreted as `oblique 10deg`.
  // If mis-parsed, it would override the previous valid declaration.
  let node = styled_div(r#"<div style="font-style: italic; font-style: oblique10deg;"></div>"#);
  assert_eq!(node.styles.font_style, FontStyle::Italic);
}

#[test]
fn font_shorthand_parses_oblique_without_angle() {
  let node = styled_div(r#"<div style="font: oblique 12px serif;"></div>"#);
  assert_eq!(node.styles.font_style, FontStyle::Oblique(None));
}

#[test]
fn font_shorthand_rejects_stray_angle_tokens() {
  // Angles are only valid in the shorthand immediately after the `oblique` keyword.
  // If we accidentally ignore them, the invalid shorthand could override the previous declaration.
  let node =
    styled_div(r#"<div style="font: italic 16px serif; font: italic 20deg 12px serif;"></div>"#);
  assert_eq!(node.styles.font_style, FontStyle::Italic);
  assert!((node.styles.font_size - 16.0).abs() < 1e-6);
}

