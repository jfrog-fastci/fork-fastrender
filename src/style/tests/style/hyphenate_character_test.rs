use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;

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

fn hyphenate_character(node: &StyledNode) -> Option<&str> {
  node.styles.hyphenate_character.as_deref()
}

#[test]
fn hyphenate_character_parses_string() {
  let dom = dom::parse_html(r#"<div style="hyphenate-character: '*';">text</div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");
  assert_eq!(hyphenate_character(node), Some("*"));
}

#[test]
fn hyphenate_character_parses_auto() {
  let dom = dom::parse_html(r#"<div style="hyphenate-character: auto;">text</div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");
  assert_eq!(hyphenate_character(node), None);
}

#[test]
fn hyphenate_character_inherits() {
  let dom = dom::parse_html(r#"<div style="hyphenate-character: '*';"><span>child</span></div>"#)
    .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");
  assert_eq!(hyphenate_character(parent), Some("*"));
  assert_eq!(hyphenate_character(child), Some("*"));
}

