use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_styled_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }

  node
    .children
    .iter()
    .find_map(|child| find_styled_by_id(child, id))
}

#[test]
fn font_size_unitless_zero_is_applied() {
  let dom = dom::parse_html(
    r#"
      <!doctype html>
      <div id="target">Bloomberg</div>
    "#,
  )
  .expect("parse html");

  let stylesheet = parse_stylesheet("#target { font-size: 0; }").expect("stylesheet parses");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let target = find_styled_by_id(&styled, "target").expect("target styled");

  assert_eq!(target.styles.font_size, 0.0);
}
