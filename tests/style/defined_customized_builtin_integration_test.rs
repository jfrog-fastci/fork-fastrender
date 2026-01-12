use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{
  apply_styles_with_media_target_and_imports_with_options, CascadeOptions, StyledNode,
};
use fastrender::style::media::MediaContext;
use fastrender::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn defined_customized_builtin_integration_test() {
  let dom = dom::parse_html(
    r#"
      <button id="b" is="x-foo"></button>
      <button id="b2" is="notcustom"></button>
      <x-foo id="x"></x-foo>
      <div id="d"></div>
    "#,
  )
  .expect("parse html");
  let stylesheet = parse_stylesheet(
    r"
      #b:defined { color: rgb(0 128 0); }
      #b:not(:defined) { color: rgb(255 0 0); }

      #b2:defined { color: rgb(0 128 0); }
      #b2:not(:defined) { color: rgb(255 0 0); }

      #x:defined { color: rgb(0 128 0); }
      #x:not(:defined) { color: rgb(255 0 0); }

      #d:defined { color: rgb(0 128 0); }
      #d:not(:defined) { color: rgb(255 0 0); }
    ",
  )
  .expect("parse stylesheet");
  let media_ctx = MediaContext::screen(800.0, 600.0);

  let styled_default = apply_styles_with_media_target_and_imports_with_options(
    &dom,
    &stylesheet,
    &media_ctx,
    None,
    None,
    None,
    None,
    None,
    None,
    CascadeOptions::default(),
  );
  let b_default = find_by_id(&styled_default, "b").expect("node #b");
  assert_eq!(
    b_default.styles.color,
    Rgba::rgb(0, 128, 0),
    "compat mode should treat customized built-ins as :defined"
  );
  let b2_default = find_by_id(&styled_default, "b2").expect("node #b2");
  assert_eq!(b2_default.styles.color, Rgba::rgb(0, 128, 0));
  let x_default = find_by_id(&styled_default, "x").expect("node #x");
  assert_eq!(x_default.styles.color, Rgba::rgb(0, 128, 0));
  let d_default = find_by_id(&styled_default, "d").expect("node #d");
  assert_eq!(d_default.styles.color, Rgba::rgb(0, 128, 0));

  let styled_spec = apply_styles_with_media_target_and_imports_with_options(
    &dom,
    &stylesheet,
    &media_ctx,
    None,
    None,
    None,
    None,
    None,
    None,
    CascadeOptions::default().with_custom_elements_defined(false),
  );
  let b_spec = find_by_id(&styled_spec, "b").expect("node #b");
  assert_eq!(
    b_spec.styles.color,
    Rgba::rgb(255, 0, 0),
    "customized built-ins should be treated as undefined when custom elements are not run"
  );
  let x_spec = find_by_id(&styled_spec, "x").expect("node #x");
  assert_eq!(
    x_spec.styles.color,
    Rgba::rgb(255, 0, 0),
    "autonomous custom elements should be treated as undefined when custom elements are not run"
  );
  let b2_spec = find_by_id(&styled_spec, "b2").expect("node #b2");
  assert_eq!(
    b2_spec.styles.color,
    Rgba::rgb(0, 128, 0),
    "elements with invalid `is` values should remain :defined"
  );
  let d_spec = find_by_id(&styled_spec, "d").expect("node #d");
  assert_eq!(d_spec.styles.color, Rgba::rgb(0, 128, 0));
}

