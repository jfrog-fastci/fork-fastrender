use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::Appearance;
use crate::style::values::Length;
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
fn appearance_none_strips_ua_border_background_and_padding() {
  let html = r#"
    <form>
      <input id="cb" type="checkbox" style="appearance:none">
      <input id="txt" type="text" style="appearance:none">
    </form>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet("").expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));

  let checkbox = find_by_id(&styled, "cb").expect("checkbox node");
  assert!(
    matches!(checkbox.styles.appearance, Appearance::None),
    "expected checkbox to compute appearance:none; got {:?}",
    checkbox.styles.appearance
  );
  assert_eq!(checkbox.styles.used_border_top_width(), Length::px(0.0));
  assert_eq!(checkbox.styles.used_border_right_width(), Length::px(0.0));
  assert_eq!(checkbox.styles.used_border_bottom_width(), Length::px(0.0));
  assert_eq!(checkbox.styles.used_border_left_width(), Length::px(0.0));
  assert_eq!(checkbox.styles.background_color, Rgba::TRANSPARENT);

  let text = find_by_id(&styled, "txt").expect("text input node");
  assert!(
    matches!(text.styles.appearance, Appearance::None),
    "expected text input to compute appearance:none; got {:?}",
    text.styles.appearance
  );
  assert_eq!(text.styles.used_border_top_width(), Length::px(0.0));
  assert_eq!(text.styles.used_border_right_width(), Length::px(0.0));
  assert_eq!(text.styles.used_border_bottom_width(), Length::px(0.0));
  assert_eq!(text.styles.used_border_left_width(), Length::px(0.0));
  assert_eq!(text.styles.background_color, Rgba::TRANSPARENT);
  assert_eq!(text.styles.padding_top, Length::px(0.0));
  assert_eq!(text.styles.padding_right, Length::px(0.0));
  assert_eq!(text.styles.padding_bottom, Length::px(0.0));
  assert_eq!(text.styles.padding_left, Length::px(0.0));
}

