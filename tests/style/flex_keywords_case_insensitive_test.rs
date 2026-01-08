use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::JustifyContent;

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
fn flexbox_keyword_values_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <div id="direct"></div>
      <div id="place"></div>
      <div id="auto"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #direct {
        display: flex;
        flex-direction: COLUMN-REVERSE;
        flex-wrap: WRAP-REVERSE;
        justify-content: SPACE-BETWEEN;
        align-items: FLEX-END;
        align-self: CENTER;
        align-content: SPACE-AROUND;
        justify-items: END;
        justify-self: START;
        flex-basis: CONTENT;
      }

      #place {
        place-items: CENTER END;
        place-content: SPACE-AROUND CENTER;
      }

      #auto {
        align-self: CENTER;
        align-self: AUTO;
        justify-self: END;
        justify-self: AUTO;
      }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let direct = find_by_id(&styled, "direct").expect("direct element");
  assert_eq!(direct.styles.flex_direction, FlexDirection::ColumnReverse);
  assert_eq!(direct.styles.flex_wrap, FlexWrap::WrapReverse);
  assert_eq!(direct.styles.justify_content, JustifyContent::SpaceBetween);
  assert_eq!(direct.styles.align_items, AlignItems::FlexEnd);
  assert_eq!(direct.styles.align_self, Some(AlignItems::Center));
  assert_eq!(direct.styles.align_content, AlignContent::SpaceAround);
  assert_eq!(direct.styles.justify_items, AlignItems::End);
  assert_eq!(direct.styles.justify_self, Some(AlignItems::Start));
  assert_eq!(direct.styles.flex_basis, FlexBasis::Content);

  let place = find_by_id(&styled, "place").expect("place element");
  assert_eq!(place.styles.align_items, AlignItems::Center);
  assert_eq!(place.styles.justify_items, AlignItems::End);
  assert_eq!(place.styles.align_content, AlignContent::SpaceAround);
  assert_eq!(place.styles.justify_content, JustifyContent::Center);

  let auto = find_by_id(&styled, "auto").expect("auto element");
  assert_eq!(auto.styles.align_self, None);
  assert_eq!(auto.styles.justify_self, None);
}

