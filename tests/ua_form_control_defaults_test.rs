use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::BoxSizing;
use fastrender::style::types::CursorKeyword;
use fastrender::style::types::Overflow;
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
      <select id="select-default"><option>One</option></select>
      <select id="select-size-empty" size><option>One</option></select>
      <select id="select-size-0" size="0"><option>One</option></select>
      <select id="select-size-1" size="1"><option>One</option></select>
      <select id="select-size-3" size="3"><option>One</option></select>
      <select id="select-multiple" multiple><option>One</option></select>
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

  let select_default = find_by_id(&styled, "select-default").expect("default select node");
  assert_eq!(select_default.styles.padding_right, Length::px(20.0));
  assert_eq!(select_default.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_default.styles.overflow_x, Overflow::Clip);
  assert_eq!(select_default.styles.overflow_y, Overflow::Clip);

  let select_size_empty = find_by_id(&styled, "select-size-empty").expect("size-empty select node");
  assert_eq!(select_size_empty.styles.padding_right, Length::px(20.0));
  assert_eq!(select_size_empty.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_size_empty.styles.overflow_x, Overflow::Clip);
  assert_eq!(select_size_empty.styles.overflow_y, Overflow::Clip);

  let select_size_0 = find_by_id(&styled, "select-size-0").expect("size-0 select node");
  assert_eq!(select_size_0.styles.padding_right, Length::px(20.0));
  assert_eq!(select_size_0.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_size_0.styles.overflow_x, Overflow::Clip);
  assert_eq!(select_size_0.styles.overflow_y, Overflow::Clip);

  let select_size_1 = find_by_id(&styled, "select-size-1").expect("size-1 select node");
  assert_eq!(select_size_1.styles.padding_right, Length::px(20.0));
  assert_eq!(select_size_1.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_size_1.styles.overflow_x, Overflow::Clip);
  assert_eq!(select_size_1.styles.overflow_y, Overflow::Clip);

  let select_size_3 = find_by_id(&styled, "select-size-3").expect("size-3 select node");
  assert_eq!(select_size_3.styles.padding_right, Length::px(6.0));
  assert_eq!(select_size_3.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_size_3.styles.overflow_x, Overflow::Hidden);
  assert_eq!(select_size_3.styles.overflow_y, Overflow::Auto);

  let select_multiple = find_by_id(&styled, "select-multiple").expect("multiple select node");
  assert_eq!(select_multiple.styles.padding_right, Length::px(6.0));
  assert_eq!(select_multiple.styles.cursor, CursorKeyword::Default);
  assert_eq!(select_multiple.styles.overflow_x, Overflow::Hidden);
  assert_eq!(select_multiple.styles.overflow_y, Overflow::Auto);

  let submit = find_by_id(&styled, "submit").expect("submit input node");
  assert_eq!(submit.styles.cursor, CursorKeyword::Pointer);

  let disabled = find_by_id(&styled, "disabled").expect("disabled input node");
  assert_eq!(disabled.styles.cursor, CursorKeyword::Default);
}
