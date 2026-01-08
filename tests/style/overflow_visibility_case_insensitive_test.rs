use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::computed::Visibility;
use fastrender::style::types::Overflow;
use fastrender::style::types::WebkitBoxOrient;

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
fn overflow_and_visibility_keywords_are_ascii_case_insensitive() {
  let dom = dom::parse_html(
    r#"
      <div id="overflow"></div>
      <div id="clip"></div>
      <div id="axes"></div>
      <div id="visibility"></div>
      <div id="orient"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #overflow { overflow: HIDDEN; }
      #clip { overflow: CLIP; }
      #axes { overflow-x: SCROLL; overflow-y: HIDDEN; }
      #visibility { visibility: HIDDEN; }
      #orient { box-orient: VERTICAL; }
    "#,
  )
  .expect("stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let overflow = find_by_id(&styled, "overflow").expect("overflow element");
  assert_eq!(overflow.styles.overflow_x, Overflow::Hidden);
  assert_eq!(overflow.styles.overflow_y, Overflow::Hidden);

  let clip = find_by_id(&styled, "clip").expect("clip element");
  assert_eq!(clip.styles.overflow_x, Overflow::Clip);
  assert_eq!(clip.styles.overflow_y, Overflow::Clip);

  let axes = find_by_id(&styled, "axes").expect("axes element");
  assert_eq!(axes.styles.overflow_x, Overflow::Scroll);
  assert_eq!(axes.styles.overflow_y, Overflow::Hidden);

  let visibility = find_by_id(&styled, "visibility").expect("visibility element");
  assert_eq!(visibility.styles.visibility, Visibility::Hidden);

  let orient = find_by_id(&styled, "orient").expect("orient element");
  assert_eq!(orient.styles.webkit_box_orient, WebkitBoxOrient::Vertical);
}
