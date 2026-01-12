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
fn defined_pseudo_cascade_toggle() {
  let dom = dom::parse_html("<x-foo id=t></x-foo><div id=c></div>").expect("parse html");
  let stylesheet = parse_stylesheet(
    r"
      #t:defined { color: rgb(0 128 0); }
      #t:not(:defined) { color: rgb(255 0 0); }

      #c:defined { color: rgb(0 0 255); }
      #c:not(:defined) { color: rgb(255 0 0); }
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
  let t_default = find_by_id(&styled_default, "t").expect("node #t");
  assert_eq!(t_default.styles.color, Rgba::rgb(0, 128, 0));
  let c_default = find_by_id(&styled_default, "c").expect("node #c");
  assert_eq!(
    c_default.styles.color,
    Rgba::rgb(0, 0, 255),
    "non-custom-element tags should always be :defined"
  );

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
  let t_spec = find_by_id(&styled_spec, "t").expect("node #t");
  assert_eq!(
    t_spec.styles.color,
    Rgba::rgb(255, 0, 0),
    "custom elements should be treated as undefined when custom elements are not run"
  );
  let c_spec = find_by_id(&styled_spec, "c").expect("node #c");
  assert_eq!(
    c_spec.styles.color,
    Rgba::rgb(0, 0, 255),
    "non-custom-element tags should always be :defined"
  );
}

