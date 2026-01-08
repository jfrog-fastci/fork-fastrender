use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::ListStylePosition;
use fastrender::style::types::ListStyleType;
use fastrender::style::types::OutlineStyle;
use fastrender::style::values::Length;

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
fn list_border_outline_and_flex_keywords_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <ul id="list"></ul>
      <div id="border"></div>
      <div id="outline"></div>
      <div id="flex"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #list { list-style-type: NONE; list-style-position: INSIDE; }
      #border { border-style: SOLID DOTTED; }
      #outline { outline-width: THICK; outline: THIN DOTTED red; }
      #flex { flex: AUTO; }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let list = find_by_id(&styled, "list").expect("list element");
  assert_eq!(list.styles.list_style_type, ListStyleType::None);
  assert_eq!(list.styles.list_style_position, ListStylePosition::Inside);

  let border = find_by_id(&styled, "border").expect("border element");
  assert_eq!(border.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(border.styles.border_right_style, BorderStyle::Dotted);
  assert_eq!(border.styles.border_bottom_style, BorderStyle::Solid);
  assert_eq!(border.styles.border_left_style, BorderStyle::Dotted);

  let outline = find_by_id(&styled, "outline").expect("outline element");
  assert_eq!(outline.styles.outline_width, Length::px(1.0));
  assert_eq!(outline.styles.outline_style, OutlineStyle::Dotted);

  let flex = find_by_id(&styled, "flex").expect("flex element");
  assert_eq!(flex.styles.flex_grow, 1.0);
  assert_eq!(flex.styles.flex_shrink, 1.0);
  assert_eq!(flex.styles.flex_basis, FlexBasis::Auto);
}

