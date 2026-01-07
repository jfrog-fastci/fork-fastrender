use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::BoxSizing;
use fastrender::style::types::CursorKeyword;
use fastrender::Length;

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
fn form_control_defaults_come_from_user_agent_stylesheet() {
  let html = r#"
    <form>
      <input id="text" type="text">
      <textarea id="textarea"></textarea>
      <input id="submit" type="submit">
      <input id="disabled" type="text" disabled>
    </form>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet("").expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));

  let input = find_by_id(&styled, "text").expect("text input node");
  assert_eq!(input.styles.padding_top, Length::px(4.0));
  assert_eq!(input.styles.padding_right, Length::px(6.0));
  assert_eq!(input.styles.padding_bottom, Length::px(4.0));
  assert_eq!(input.styles.padding_left, Length::px(6.0));

  assert_eq!(input.styles.border_top_width, Length::px(1.0));
  assert_eq!(input.styles.border_right_width, Length::px(1.0));
  assert_eq!(input.styles.border_bottom_width, Length::px(1.0));
  assert_eq!(input.styles.border_left_width, Length::px(1.0));

  assert_eq!(input.styles.border_top_style, BorderStyle::Solid);
  assert_eq!(input.styles.border_right_style, BorderStyle::Solid);
  assert_eq!(input.styles.border_bottom_style, BorderStyle::Solid);
  assert_eq!(input.styles.border_left_style, BorderStyle::Solid);

  assert_eq!(input.styles.box_sizing, BoxSizing::BorderBox);
  assert_eq!(input.styles.cursor, CursorKeyword::Text);

  let textarea = find_by_id(&styled, "textarea").expect("textarea node");
  assert_eq!(textarea.styles.cursor, CursorKeyword::Text);

  let submit = find_by_id(&styled, "submit").expect("submit input node");
  assert_eq!(submit.styles.cursor, CursorKeyword::Pointer);

  let disabled = find_by_id(&styled, "disabled").expect("disabled input node");
  assert_eq!(disabled.styles.cursor, CursorKeyword::Default);
}

