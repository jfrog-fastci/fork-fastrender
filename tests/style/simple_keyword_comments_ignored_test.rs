use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::ListStyleType;
use fastrender::Display;

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

fn styled(html: &str, tag: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, tag).unwrap_or_else(|| panic!("{tag}")).clone()
}

#[test]
fn trailing_comments_do_not_break_single_ident_keyword_parsing() {
  let div = styled(
    r#"<div style="display: inline; display: block/*comment*/;"></div>"#,
    "div",
  );
  assert_eq!(div.styles.display, Display::Block);

  let ol = styled(
    r#"<ol style="list-style-type: square; list-style-type: decimal/*comment*/;"></ol>"#,
    "ol",
  );
  assert!(matches!(ol.styles.list_style_type, ListStyleType::Decimal));
}

#[test]
fn comments_cannot_split_identifiers_into_new_keywords() {
  let div = styled(
    r#"<div style="display: inline; display: bl/*comment*/ock;"></div>"#,
    "div",
  );
  assert_eq!(div.styles.display, Display::Inline);

  let ol = styled(
    r#"<ol style="list-style-type: square; list-style-type: dec/*comment*/imal;"></ol>"#,
    "ol",
  );
  assert!(matches!(ol.styles.list_style_type, ListStyleType::Square));
}

