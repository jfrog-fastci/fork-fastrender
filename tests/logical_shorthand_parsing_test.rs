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
fn parses_inset_shorthand_four_values() {
  let css = "div { position: absolute; inset: 1px 2px 3px 4px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.top, Some(Length::px(1.0)));
  assert_eq!(div.styles.right, Some(Length::px(2.0)));
  assert_eq!(div.styles.bottom, Some(Length::px(3.0)));
  assert_eq!(div.styles.left, Some(Length::px(4.0)));
}

#[test]
fn parses_margin_inline_two_values() {
  let css = "div { margin-inline: 5px 6px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.margin_left, Some(Length::px(5.0)));
  assert_eq!(div.styles.margin_right, Some(Length::px(6.0)));
}

#[test]
fn parses_padding_inline_two_values() {
  let css = "div { padding-inline: 7px 8px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.padding_left, Length::px(7.0));
  assert_eq!(div.styles.padding_right, Length::px(8.0));
}

#[test]
fn parses_border_inline_width_two_values() {
  let css = "div { border-inline-width: 9px 10px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_width, Length::px(9.0));
  assert_eq!(div.styles.border_right_width, Length::px(10.0));
}

#[test]
fn parses_border_inline_style_two_values() {
  let css = "div { border-inline-style: solid dashed; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Dashed);
}

#[test]
fn parses_border_block_style_two_values() {
  let css = "div { border-block-style: dotted solid; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_style, BorderStyle::Dotted);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::Solid);
}

#[test]
fn parses_border_inline_color_two_values() {
  let css = "div { border-inline-color: rgb(255, 0, 0) rgb(0, 0, 255); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_color, Rgba::new(255, 0, 0, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(0, 0, 255, 1.0));
}

