use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::Rgba;

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
fn parses_border_color_shorthand_four_values() {
  let css = "div { border-color: rgb(1, 2, 3) rgb(4, 5, 6) rgb(7, 8, 9) rgb(10, 11, 12); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(4, 5, 6, 1.0));
  assert_eq!(div.styles.border_bottom_color, Rgba::new(7, 8, 9, 1.0));
  assert_eq!(div.styles.border_left_color, Rgba::new(10, 11, 12, 1.0));
}

#[test]
fn parses_border_color_shorthand_two_values() {
  let css = "div { border-color: rgb(1, 2, 3) rgb(4, 5, 6); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_bottom_color, Rgba::new(1, 2, 3, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(4, 5, 6, 1.0));
  assert_eq!(div.styles.border_left_color, Rgba::new(4, 5, 6, 1.0));
}

#[test]
fn parses_border_inline_color_single_value() {
  let css = "div { border-inline-color: rgb(13, 14, 15); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_left_color, Rgba::new(13, 14, 15, 1.0));
  assert_eq!(div.styles.border_right_color, Rgba::new(13, 14, 15, 1.0));
}

#[test]
fn parses_border_block_color_single_value() {
  let css = "div { border-block-color: rgb(16, 17, 18); }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.border_top_color, Rgba::new(16, 17, 18, 1.0));
  assert_eq!(div.styles.border_bottom_color, Rgba::new(16, 17, 18, 1.0));
}
