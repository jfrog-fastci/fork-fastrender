use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, ReplacedType, SvgContent};

fn find_inline_svg_content(node: &BoxNode) -> Option<&SvgContent> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::Svg { content } = &replaced.replaced_type {
      return Some(content);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_inline_svg_content(child) {
      return Some(found);
    }
  }
  None
}

fn g_style_from_serialized_svg(svg: &str) -> Option<(Option<String>, Option<String>)> {
  let doc = roxmltree::Document::parse(svg).ok()?;
  let g = doc
    .descendants()
    .find(|node| node.has_tag_name("g") && node.attribute("id").is_some_and(|id| id == "g"))?;
  Some((
    g.attribute("transform").map(|v| v.to_string()),
    g.attribute("style").map(|v| v.to_string()),
  ))
}

#[test]
fn svg_transform_percentage_falls_back_to_css_style_text() {
  let html = r#"
    <html>
      <body>
        <svg>
          <g id="g">
            <rect id="r" x="0" y="0" width="10" height="10"></rect>
          </g>
        </svg>
      </body>
    </html>
  "#;
  let stylesheet = parse_stylesheet("g{ transform: translateX(100%); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled).expect("box tree");
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  assert!(
    transform_attr.is_none(),
    "expected percentage transforms to be serialized as CSS text, not an SVG transform attribute"
  );
  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percent length: {style_attr:?}"
  );
}

#[test]
fn svg_transform_calc_falls_back_to_css_style_text() {
  let html = r#"
    <html>
      <body>
        <svg>
          <g id="g">
            <rect id="r" x="0" y="0" width="10" height="10"></rect>
          </g>
        </svg>
      </body>
    </html>
  "#;
  let stylesheet =
    parse_stylesheet("g{ transform: translateX(calc(100% + 2px)); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled).expect("box tree");
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  assert!(
    transform_attr.is_none(),
    "expected calc transforms to be serialized as CSS text, not an SVG transform attribute"
  );
  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("calc("),
    "expected serialized g style to preserve calc() text: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percentage inside calc(): {style_attr:?}"
  );
}

#[test]
fn svg_transform_unserializable_keeps_authored_transform_attribute() {
  let html = r#"
    <html>
      <body>
        <svg>
          <g id="g" transform="translate(10 0)">
            <rect id="r" x="0" y="0" width="10" height="10"></rect>
          </g>
        </svg>
      </body>
    </html>
  "#;
  let stylesheet = parse_stylesheet("g{ transform: translateX(100%); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled).expect("box tree");
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  let transform_attr = transform_attr.expect("authored transform attribute");
  assert!(
    transform_attr.contains("translate(10"),
    "expected authored transform attribute to be preserved: {transform_attr:?}"
  );

  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percent length: {style_attr:?}"
  );
}
