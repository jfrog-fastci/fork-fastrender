use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn blank_matches_empty_or_whitespace_only_form_controls() {
  let html = r#"
    <input id="input_empty">
    <input id="input_ws" value="   ">
    <input id="input_text" value="x">
    <textarea id="textarea_ws">   </textarea>
    <input id="checkbox" type="checkbox">
  "#;
  let css = r#"
    input, textarea { color: rgb(9 9 9); }
    :blank { color: rgb(1 2 3); }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    find_by_id(&styled, "input_empty")
      .expect("input_empty")
      .styles
      .color,
    Rgba::rgb(1, 2, 3)
  );
  assert_eq!(
    find_by_id(&styled, "input_ws").expect("input_ws").styles.color,
    Rgba::rgb(1, 2, 3)
  );
  assert_eq!(
    find_by_id(&styled, "input_text")
      .expect("input_text")
      .styles
      .color,
    Rgba::rgb(9, 9, 9)
  );
  assert_eq!(
    find_by_id(&styled, "textarea_ws")
      .expect("textarea_ws")
      .styles
      .color,
    Rgba::rgb(1, 2, 3)
  );
  assert_eq!(
    find_by_id(&styled, "checkbox").expect("checkbox").styles.color,
    Rgba::rgb(9, 9, 9)
  );
}

