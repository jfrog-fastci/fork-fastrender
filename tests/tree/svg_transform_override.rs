use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, ReplacedType, SvgContent};

fn find_inline_svg(node: &BoxNode) -> Option<&SvgContent> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::Svg { content } = &replaced.replaced_type {
      return Some(content);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_inline_svg(child) {
      return Some(found);
    }
  }
  None
}

fn serialized_inline_svg(svg_markup: &str, stylesheet: &StyleSheet) -> String {
  let html = format!("<html><body>{}</body></html>", svg_markup);
  let dom = dom::parse_html(&html).expect("parse html");
  let styled = cascade::apply_styles(&dom, stylesheet);
  let tree = generate_box_tree(&styled).expect("box tree");
  find_inline_svg(&tree.root)
    .expect("inline svg replaced box")
    .svg
    .clone()
}

#[test]
fn css_transform_overrides_svg_transform_attribute_in_serialized_svg() {
  let svg_markup = r#"
    <svg>
      <g id="g" transform="translate(200 0)">
        <rect width="10" height="10"></rect>
      </g>
    </svg>
  "#;
  let stylesheet = parse_stylesheet("g { transform: translate(100px, 0px); }").unwrap();
  let serialized = serialized_inline_svg(svg_markup, &stylesheet);
  let doc = roxmltree::Document::parse(&serialized).expect("parse serialized svg");
  let g = doc
    .descendants()
    .find(|node| node.is_element() && node.attribute("id") == Some("g"))
    .expect("g element");
  assert_eq!(g.attribute("transform"), Some("translate(100 0)"));
}

#[test]
fn css_transform_none_removes_svg_transform_attribute_in_serialized_svg() {
  let svg_markup = r#"
    <svg>
      <g id="g" transform="translate(200 0)">
        <rect width="10" height="10"></rect>
      </g>
    </svg>
  "#;
  let stylesheet = parse_stylesheet("g { transform: none; }").unwrap();
  let serialized = serialized_inline_svg(svg_markup, &stylesheet);
  let doc = roxmltree::Document::parse(&serialized).expect("parse serialized svg");
  let g = doc
    .descendants()
    .find(|node| node.is_element() && node.attribute("id") == Some("g"))
    .expect("g element");
  assert!(g.attribute("transform").is_none());
}
