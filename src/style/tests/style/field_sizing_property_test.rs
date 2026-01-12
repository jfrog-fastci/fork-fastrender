use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::FieldSizing;

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

#[test]
fn field_sizing_parses_fixed_and_content() {
  let dom = dom::parse_html(
    r#"
      <div></div>
      <input style="field-sizing: content">
      <textarea style="field-sizing: content; field-sizing: fixed"></textarea>
    "#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let input = find_first(&styled, "input").expect("input");
  assert_eq!(input.styles.field_sizing, FieldSizing::Content);

  let textarea = find_first(&styled, "textarea").expect("textarea");
  assert_eq!(textarea.styles.field_sizing, FieldSizing::Fixed);
}

#[test]
fn field_sizing_invalid_value_is_ignored() {
  let dom =
    dom::parse_html(r#"<input style="field-sizing: content; field-sizing: no-such-value">"#)
      .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let input = find_first(&styled, "input").expect("input");
  assert_eq!(input.styles.field_sizing, FieldSizing::Content);
}
