use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BorderStyle;
use fastrender::Length;

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
fn border_style_defaults_width_to_medium_when_style_provided() {
  let css = "div { border-style: solid; }";
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
}

#[test]
fn border_style_defaults_width_per_side() {
  let css = "div { border-style: none solid; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_style, BorderStyle::None);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::None);
  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Solid);

  assert_eq!(div.styles.border_top_width, Length::px(0.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(0.0));
  assert_eq!(div.styles.border_left_width, Length::px(3.0));
  assert_eq!(div.styles.border_right_width, Length::px(3.0));
}

#[test]
fn border_side_style_defaults_width_to_medium_when_style_provided() {
  let css = "div { border-top-style: solid; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_width, Length::px(3.0));
  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);

  assert_eq!(div.styles.border_right_width, Length::px(0.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(0.0));
  assert_eq!(div.styles.border_left_width, Length::px(0.0));
}

#[test]
fn border_style_does_not_override_explicit_border_width() {
  let css = "div { border-width: 0; border-style: solid; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_width, Length::px(0.0));
  assert_eq!(div.styles.border_right_width, Length::px(0.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(0.0));
  assert_eq!(div.styles.border_left_width, Length::px(0.0));

  assert_eq!(div.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);
}

#[test]
fn border_inline_style_defaults_width_to_medium_when_style_provided() {
  let css = "div { border-inline-style: solid; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_width, Length::px(3.0));
  assert_eq!(div.styles.border_right_width, Length::px(3.0));
  assert_eq!(div.styles.border_left_style, BorderStyle::Solid);
  assert_eq!(div.styles.border_right_style, BorderStyle::Solid);

  assert_eq!(div.styles.border_top_width, Length::px(0.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(0.0));
}

#[test]
fn border_style_none_clears_implicit_width() {
  let css = "div { border-style: solid; border-style: none; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_style, BorderStyle::None);
  assert_eq!(div.styles.border_right_style, BorderStyle::None);
  assert_eq!(div.styles.border_bottom_style, BorderStyle::None);
  assert_eq!(div.styles.border_left_style, BorderStyle::None);

  assert_eq!(div.styles.border_top_width, Length::px(0.0));
  assert_eq!(div.styles.border_right_width, Length::px(0.0));
  assert_eq!(div.styles.border_bottom_width, Length::px(0.0));
  assert_eq!(div.styles.border_left_width, Length::px(0.0));
}
