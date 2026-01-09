use fastrender::css::parser::parse_stylesheet;
use fastrender::css::supports::supports_declaration;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::types::DynamicRangeLimit;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn render_dynamic_range_limit(css: &str) -> DynamicRangeLimit {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let target = find_by_id(&styled, "t").expect("element with id t");
  target.styles.dynamic_range_limit.clone()
}

#[test]
fn dynamic_range_limit_defaults_to_no_limit() {
  assert_eq!(render_dynamic_range_limit(""), DynamicRangeLimit::NoLimit);
}

#[test]
fn dynamic_range_limit_parses_keywords() {
  assert_eq!(
    render_dynamic_range_limit(r#"#t { dynamic-range-limit: standard; }"#),
    DynamicRangeLimit::Standard
  );
  assert_eq!(
    render_dynamic_range_limit(r#"#t { dynamic-range-limit: constrained; }"#),
    DynamicRangeLimit::Constrained
  );
  assert_eq!(
    render_dynamic_range_limit(r#"#t { dynamic-range-limit: no-limit; }"#),
    DynamicRangeLimit::NoLimit
  );
}

#[test]
fn dynamic_range_limit_is_inherited() {
  let dom = dom::parse_html(r#"<div id="p"><span id="c"></span></div>"#).unwrap();
  let stylesheet = parse_stylesheet(r#"#p { dynamic-range-limit: constrained; }"#).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let parent = find_by_id(&styled, "p").expect("parent");
  let child = find_by_id(&styled, "c").expect("child");
  assert_eq!(parent.styles.dynamic_range_limit, DynamicRangeLimit::Constrained);
  assert_eq!(child.styles.dynamic_range_limit, DynamicRangeLimit::Constrained);
}

#[test]
fn dynamic_range_limit_parses_mix_function() {
  let value = render_dynamic_range_limit(
    r#"#t { dynamic-range-limit: dynamic-range-limit-mix(standard 25%, constrained 75%); }"#,
  );
  let DynamicRangeLimit::Mix(components) = value else {
    panic!("expected mix, got {value:?}");
  };
  assert_eq!(components.len(), 2);
  assert!(matches!(components[0].value.as_ref(), DynamicRangeLimit::Standard));
  assert!((components[0].percentage - 25.0).abs() < 1e-6);
  assert!(matches!(
    components[1].value.as_ref(),
    DynamicRangeLimit::Constrained
  ));
  assert!((components[1].percentage - 75.0).abs() < 1e-6);
}

#[test]
fn supports_dynamic_range_limit_declaration() {
  assert!(supports_declaration("dynamic-range-limit", "standard"));
  assert!(supports_declaration(
    "dynamic-range-limit",
    "dynamic-range-limit-mix(standard 25%, constrained 75%)"
  ));
  assert!(!supports_declaration("dynamic-range-limit", "bogus"));
}

