use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::color::Rgba;
use crate::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
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
fn default_pseudo_class_matches_default_form_controls() {
  let html = r#"
    <form id="f">
      <button id="b1"></button>
      <button id="b2" type="submit"></button>
    </form>
    <select id="s">
      <option id="o1" selected></option>
      <option id="o2"></option>
    </select>
    <input id="c" type="checkbox" checked>
  "#;

  let css = r#"
    #b1, #b2, #o1, #o2, #c { color: red; }
    #b1:default, #b2:default, #o1:default, #o2:default, #c:default { color: rgb(0, 255, 0); }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    find_by_id(&styled, "b1").expect("button#b1").styles.color,
    Rgba::GREEN,
    "first submit button should match :default"
  );
  assert_eq!(
    find_by_id(&styled, "b2").expect("button#b2").styles.color,
    Rgba::RED,
    "subsequent submit buttons should not match :default"
  );
  assert_eq!(
    find_by_id(&styled, "o1").expect("option#o1").styles.color,
    Rgba::GREEN,
    "selected option should match :default"
  );
  assert_eq!(
    find_by_id(&styled, "o2").expect("option#o2").styles.color,
    Rgba::RED,
    "unselected option should not match :default"
  );
  assert_eq!(
    find_by_id(&styled, "c").expect("input#c").styles.color,
    Rgba::GREEN,
    "checked checkbox should match :default"
  );
}
