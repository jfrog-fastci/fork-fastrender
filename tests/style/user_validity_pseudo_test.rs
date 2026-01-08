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
fn user_invalid_does_not_match_without_user_validity_hint() {
  let html = r#"
    <input id='r' required>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "r").expect("required input")),
    "inline",
    ":invalid should match but :user-invalid should not without a hint"
  );
}

#[test]
fn user_invalid_matches_when_control_user_validity_hint_set() {
  let html = r#"
    <input id='r' required data-fastr-user-validity='true'>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "r").expect("required input")),
    "block"
  );
}

#[test]
fn user_invalid_matches_when_form_user_validity_hint_set() {
  let html = r#"
    <form data-fastr-user-validity='true'>
      <input id='in' required>
    </form>
    <input id='out' required>
  "#;
  let css = r#"
    input:invalid { display: inline; }
    input:user-invalid { display: block; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "in").expect("input in form")), "block");
  assert_eq!(
    display(find_by_id(&styled, "out").expect("input outside form")),
    "inline",
    "form hint should not apply to unrelated controls"
  );
}

