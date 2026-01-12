use fastrender::css::parser::extract_css;
use fastrender::dom;
use fastrender::style::cascade;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, ReplacedType, SvgContent};

fn find_inline_svg(node: &BoxNode) -> Option<&SvgContent> {
  if let BoxType::Replaced(repl) = &node.box_type {
    if let ReplacedType::Svg { content } = &repl.replaced_type {
      return Some(content);
    }
  }
  for child in &node.children {
    if let Some(found) = find_inline_svg(child) {
      return Some(found);
    }
  }
  None
}

fn serialized_inline_svg(html: &str) -> String {
  let html = format!("<html><body>{}</body></html>", html);
  let dom = dom::parse_html(&html).expect("parse html");
  let stylesheet = extract_css(&dom).expect("extract css");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled).expect("box tree");
  find_inline_svg(&tree.root)
    .expect("inline svg replaced box")
    .svg
    .clone()
}

#[test]
fn svg_serialization_overrides_pattern_transform_with_pattern_transform_attribute() {
  let svg = serialized_inline_svg(
    r#"
    <style>pattern{transform:translate(100px,0px)}</style>
    <svg width="10" height="10">
      <defs>
        <pattern id="p" patternTransform="translate(200 0)" width="10" height="10" patternUnits="userSpaceOnUse">
          <rect width="10" height="10" fill="red" />
        </pattern>
      </defs>
      <rect width="10" height="10" fill="url(#p)" />
    </svg>
    "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let pattern = doc
    .descendants()
    .find(|node| node.is_element() && node.tag_name().name().eq_ignore_ascii_case("pattern"))
    .expect("pattern element");

  assert_eq!(
    pattern.attribute("patternTransform"),
    Some("translate(100 0)"),
    "expected CSS transform to override patternTransform during serialization"
  );
  assert!(
    pattern.attribute("transform").is_none(),
    "pattern elements must not receive a transform= attribute"
  );
}

#[test]
fn svg_serialization_transform_none_removes_pattern_transform_attribute() {
  let svg = serialized_inline_svg(
    r#"
    <style>pattern{transform:none}</style>
    <svg width="10" height="10">
      <defs>
        <pattern id="p" patternTransform="translate(200 0)" width="10" height="10" patternUnits="userSpaceOnUse">
          <rect width="10" height="10" fill="red" />
        </pattern>
      </defs>
      <rect width="10" height="10" fill="url(#p)" />
    </svg>
    "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let pattern = doc
    .descendants()
    .find(|node| node.is_element() && node.tag_name().name().eq_ignore_ascii_case("pattern"))
    .expect("pattern element");

  assert!(
    pattern.attribute("patternTransform").is_none(),
    "expected transform:none to cancel patternTransform during serialization"
  );
  assert!(pattern.attribute("transform").is_none());
}

#[test]
fn svg_serialization_overrides_gradient_transform_with_gradient_transform_attribute() {
  let svg = serialized_inline_svg(
    r#"
    <style>linearGradient{transform:translate(100px,0px)}</style>
    <svg width="10" height="10">
      <defs>
        <linearGradient id="g" gradientTransform="translate(200 0)">
          <stop offset="0" stop-color="red" />
          <stop offset="1" stop-color="blue" />
        </linearGradient>
      </defs>
      <rect width="10" height="10" fill="url(#g)" />
    </svg>
    "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let gradient = doc
    .descendants()
    .find(|node| {
      node.is_element()
        && node
          .tag_name()
          .name()
          .eq_ignore_ascii_case("linearGradient")
    })
    .expect("linearGradient element");

  assert_eq!(
    gradient.attribute("gradientTransform"),
    Some("translate(100 0)"),
    "expected CSS transform to override gradientTransform during serialization"
  );
  assert!(
    gradient.attribute("transform").is_none(),
    "gradient elements must not receive a transform= attribute"
  );
}

#[test]
fn svg_serialization_transform_none_removes_gradient_transform_attribute() {
  let svg = serialized_inline_svg(
    r#"
    <style>linearGradient{transform:none}</style>
    <svg width="10" height="10">
      <defs>
        <linearGradient id="g" gradientTransform="translate(200 0)">
          <stop offset="0" stop-color="red" />
          <stop offset="1" stop-color="blue" />
        </linearGradient>
      </defs>
      <rect width="10" height="10" fill="url(#g)" />
    </svg>
    "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let gradient = doc
    .descendants()
    .find(|node| {
      node.is_element()
        && node
          .tag_name()
          .name()
          .eq_ignore_ascii_case("linearGradient")
    })
    .expect("linearGradient element");

  assert!(
    gradient.attribute("gradientTransform").is_none(),
    "expected transform:none to cancel gradientTransform during serialization"
  );
  assert!(gradient.attribute("transform").is_none());
}
