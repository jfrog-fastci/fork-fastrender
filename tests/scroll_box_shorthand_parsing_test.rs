use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
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
fn parses_scroll_padding_shorthand_four_values() {
  let css = "div { scroll-padding: 1px 2px 3px 4px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.scroll_padding_top, Length::px(1.0));
  assert_eq!(div.styles.scroll_padding_right, Length::px(2.0));
  assert_eq!(div.styles.scroll_padding_bottom, Length::px(3.0));
  assert_eq!(div.styles.scroll_padding_left, Length::px(4.0));
}

#[test]
fn parses_scroll_margin_shorthand_two_values() {
  let css = "div { scroll-margin: 5px 6px; }";
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.scroll_margin_top, Length::px(5.0));
  assert_eq!(div.styles.scroll_margin_bottom, Length::px(5.0));
  assert_eq!(div.styles.scroll_margin_right, Length::px(6.0));
  assert_eq!(div.styles.scroll_margin_left, Length::px(6.0));
}
