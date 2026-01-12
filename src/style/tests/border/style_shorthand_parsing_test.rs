use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BorderStyle;

fn find_by_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_tag(child, tag) {
      return Some(found);
    }
  }
  None
}

#[test]
fn parses_border_style_shorthand_four_values() {
  let css = "div { border-style: solid dashed dotted double; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Dashed);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::Dotted);
  assert_eq!(div.styles.border_left_style, BorderStyle::Double);
}

#[test]
fn parses_border_style_shorthand_two_values() {
  let css = "div { border-style: solid dashed; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Dashed);
  assert_eq!(div.styles.border_left_style, BorderStyle::Dashed);
}
