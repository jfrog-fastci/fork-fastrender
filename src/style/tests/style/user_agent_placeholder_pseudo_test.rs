use crate::css::parser::parse_stylesheet;
use crate::css::types::StyleSheet;
use crate::dom;
use crate::style::cascade::{apply_styles, StyledNode};
use crate::style::color::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }

  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn placeholder_pseudo_defaults_to_graytext_without_opacity() {
  let dom = dom::parse_html(
    r#"
      <!doctype html>
      <html>
        <body>
          <input id="input" placeholder="Filter">
        </body>
      </html>
    "#,
  )
  .expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let input = find_by_id(&styled, "input").expect("input styled");
  let placeholder = input
    .placeholder_styles
    .as_ref()
    .expect("expected ::placeholder styles to be computed");

  assert_eq!(placeholder.color, Rgba::rgb(128, 128, 128));
  assert_eq!(placeholder.opacity, 1.0);
}

#[test]
fn author_placeholder_color_is_not_dimmed_by_user_agent_opacity() {
  let dom = dom::parse_html(
    r#"
      <!doctype html>
      <html>
        <body>
          <input id="input" placeholder="Filter">
        </body>
      </html>
    "#,
  )
  .expect("parse html");
  let stylesheet =
    parse_stylesheet("input::placeholder { color: rgb(11 22 33); }").expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let input = find_by_id(&styled, "input").expect("input styled");
  let placeholder = input
    .placeholder_styles
    .as_ref()
    .expect("expected ::placeholder styles to be computed");

  assert_eq!(placeholder.color, Rgba::rgb(11, 22, 33));
  assert_eq!(placeholder.opacity, 1.0);
}
