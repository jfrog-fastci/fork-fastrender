use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;

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

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

#[test]
fn radio_group_required_missing_marks_all_enabled_radios_invalid() {
  let html = r#"
    <input id="r1" type="radio" name="g" required>
    <input id="r2" type="radio" name="g">
    <input id="r3" type="radio" name="g">
  "#;
  let dom = dom::parse_html(html).expect("parse html");

  let css = r#"
    input { display: inline; }
    input:invalid { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "r1").expect("r1")), "block");
  assert_eq!(display(find_by_id(&styled, "r2").expect("r2")), "block");
  assert_eq!(display(find_by_id(&styled, "r3").expect("r3")), "block");

  let css = r#"
    input { display: inline; }
    input:required { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "r1").expect("r1")), "block");
  assert_eq!(display(find_by_id(&styled, "r2").expect("r2")), "inline");
  assert_eq!(display(find_by_id(&styled, "r3").expect("r3")), "inline");
}

#[test]
fn radio_group_with_checked_radio_is_not_invalid() {
  let html = r#"
    <input id="r1" type="radio" name="g" required>
    <input id="r2" type="radio" name="g" checked>
    <input id="r3" type="radio" name="g">
  "#;
  let dom = dom::parse_html(html).expect("parse html");

  let css = r#"
    input { display: inline; }
    input:invalid { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "r1").expect("r1")), "inline");
  assert_eq!(display(find_by_id(&styled, "r2").expect("r2")), "inline");
  assert_eq!(display(find_by_id(&styled, "r3").expect("r3")), "inline");
}

#[test]
fn radio_group_required_disabled_still_makes_enabled_members_invalid() {
  let html = r#"
    <input id="rd" type="radio" name="g" required disabled>
    <input id="r1" type="radio" name="g">
    <input id="r2" type="radio" name="g">
  "#;
  let dom = dom::parse_html(html).expect("parse html");

  let css = r#"
    input { display: inline; }
    input:invalid { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "rd").expect("rd")),
    "inline",
    "disabled radios are not :invalid"
  );
  assert_eq!(display(find_by_id(&styled, "r1").expect("r1")), "block");
  assert_eq!(display(find_by_id(&styled, "r2").expect("r2")), "block");
}

#[test]
fn radio_group_is_scoped_by_form_owner() {
  let html = r#"
    <form id="f1">
      <input id="f1r1" type="radio" name="g" required>
      <input id="f1r2" type="radio" name="g">
    </form>
    <form id="f2"></form>
    <input id="f2r1" type="radio" name="g" form="f2">
    <input id="f2r2" type="radio" name="g" form="f2">
  "#;
  let dom = dom::parse_html(html).expect("parse html");

  let css = r#"
    input { display: inline; }
    input:invalid { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "f1r1").expect("f1r1")), "block");
  assert_eq!(display(find_by_id(&styled, "f1r2").expect("f1r2")), "block");
  assert_eq!(
    display(find_by_id(&styled, "f2r1").expect("f2r1")),
    "inline"
  );
  assert_eq!(
    display(find_by_id(&styled, "f2r2").expect("f2r2")),
    "inline"
  );
}

