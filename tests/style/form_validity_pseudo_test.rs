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
fn form_invalid_matches_descendant_invalid_controls() {
  let html = r#"
    <form id="f">
      <input required>
    </form>
  "#;
  let css = r#"
    form:valid { display: inline; }
    form:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "f").expect("form#f")), "none");
}

#[test]
fn form_valid_matches_when_all_controls_are_valid_or_disabled() {
  let html = r#"
    <form id="valid">
      <input required value="ok">
    </form>
    <form id="disabled-only">
      <input required disabled>
    </form>
  "#;
  let css = r#"
    form:valid { display: inline; }
    form:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "valid").expect("form#valid")),
    "inline"
  );
  assert_eq!(
    display(find_by_id(&styled, "disabled-only").expect("form#disabled-only")),
    "inline",
    "disabled candidate controls do not make the form invalid"
  );
}

#[test]
fn fieldset_validity_propagates_from_descendant_controls() {
  let html = r#"
    <fieldset id="fs-invalid">
      <input required>
    </fieldset>
    <fieldset id="fs-valid">
      <input required value="ok">
    </fieldset>
  "#;
  let css = r#"
    fieldset:valid { display: inline; }
    fieldset:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "fs-invalid").expect("fieldset#fs-invalid")),
    "none"
  );
  assert_eq!(
    display(find_by_id(&styled, "fs-valid").expect("fieldset#fs-valid")),
    "inline"
  );
}

#[test]
fn form_owner_resolution_includes_form_attribute_association() {
  let html = r#"
    <form id="f"></form>
    <input form="f" required>
  "#;
  let css = r#"
    form:valid { display: inline; }
    form:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(display(find_by_id(&styled, "f").expect("form#f")), "none");
}

#[test]
fn disabled_controls_do_not_affect_fieldset_or_form_validity() {
  let html = r#"
    <form id="f">
      <fieldset id="fs" disabled>
        <input required>
      </fieldset>
    </form>
  "#;
  let css = r#"
    form:valid { display: inline; }
    form:invalid { display: none; }
    fieldset:valid { display: inline; }
    fieldset:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "f").expect("form#f")),
    "inline",
    "a disabled fieldset disables its descendants, so they should not make the form invalid"
  );
  assert_eq!(
    display(find_by_id(&styled, "fs").expect("fieldset#fs")),
    "inline",
    "disabled descendants should not make the fieldset invalid"
  );
}

#[test]
fn disabled_associated_controls_do_not_affect_form_validity() {
  let html = r#"
    <form id="f"></form>
    <input form="f" required disabled>
  "#;
  let css = r#"
    form:valid { display: inline; }
    form:invalid { display: none; }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    display(find_by_id(&styled, "f").expect("form#f")),
    "inline"
  );
}
