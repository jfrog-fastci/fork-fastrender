use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BorderStyle;
use fastrender::{Length, Rgba};

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
fn border_shorthand_defaults_width_to_medium_when_style_provided() {
  let css = "div { border: solid rgb(1, 2, 3); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_width, Length::px(3.0));
  assert_eq!(div.styles.border_right_width, Length::px(3.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(3.0));
  assert_eq!(div.styles.border_left_width, Length::px(3.0));

  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);

  assert_eq!(div.styles.border_top_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_bottom_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_left_color, Rgba::new(1, 2, 3, 1.0));
}

#[test]
fn border_side_shorthand_defaults_width_to_medium_when_style_provided() {
  let css = "div { border-top: solid rgb(4, 5, 6); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_width, Length::px(3.0));
  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_top_color, Rgba::new(4, 5, 6, 1.0));
}

#[test]
fn border_inline_shorthand_defaults_width_to_medium_when_style_provided() {
  let css = "div { border-inline: solid rgb(7, 8, 9); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_width, Length::px(3.0));
  assert_eq!(div.styles.border_right_width, Length::px(3.0));
  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_left_color, Rgba::new(7, 8, 9, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(7, 8, 9, 1.0));
}
