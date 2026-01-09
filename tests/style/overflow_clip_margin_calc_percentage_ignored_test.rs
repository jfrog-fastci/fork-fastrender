use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::{OverflowClipMargin, VisualBox};
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
fn overflow_clip_margin_rejects_calc_with_percentage() {
  let html = r#"<div id="target"></div>"#;
  let css = r#"
    #target {
      overflow-clip-margin: 10px;
      overflow-clip-margin: calc(5% + 1px);
    }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let node = find_by_id(&styled, "target").expect("target");
  assert_eq!(
    node.styles.overflow_clip_margin,
    OverflowClipMargin {
      visual_box: VisualBox::PaddingBox,
      margin: Length::px(10.0),
    }
  );
}

#[test]
fn overflow_clip_margin_rejects_negative_absolute_calc() {
  let html = r#"<div id="target"></div>"#;
  let css = r#"
    #target {
      overflow-clip-margin: 10px;
      overflow-clip-margin: calc(-5px);
    }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let node = find_by_id(&styled, "target").expect("target");
  assert_eq!(
    node.styles.overflow_clip_margin,
    OverflowClipMargin {
      visual_box: VisualBox::PaddingBox,
      margin: Length::px(10.0),
    }
  );
}
