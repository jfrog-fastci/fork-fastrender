use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::{apply_styles_with_media_and_options, CascadeOptions, StyledNode};
use crate::style::color::Rgba;
use crate::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn defined_pseudo_considers_is_attribute_for_customized_builtins() {
  let html = r#"<button id="t" is="x-foo"></button>"#;
  let css = r#"
    button[is]:defined { color: rgb(0 128 0); }
    button[is]:not(:defined) { color: rgb(255 0 0); }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::screen(800.0, 600.0);

  let styled_spec = apply_styles_with_media_and_options(
    &dom,
    &stylesheet,
    &media,
    CascadeOptions {
      treat_custom_elements_as_defined: false,
      ..CascadeOptions::default()
    },
  );
  assert_eq!(
    find_by_id(&styled_spec, "t").expect("button").styles.color,
    Rgba::rgb(255, 0, 0),
    "spec mode should treat customized built-in elements as undefined"
  );

  let styled_compat =
    apply_styles_with_media_and_options(&dom, &stylesheet, &media, CascadeOptions::default());
  assert_eq!(
    find_by_id(&styled_compat, "t").expect("button").styles.color,
    Rgba::rgb(0, 128, 0),
    "compat mode should treat customized built-in elements as defined"
  );
}
