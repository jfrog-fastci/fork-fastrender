use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, ReplacedType};

fn find_inline_svg_box(node: &BoxNode) -> Option<&BoxNode> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if matches!(replaced.replaced_type, ReplacedType::Svg { .. }) {
      return Some(node);
    }
  }
  for child in &node.children {
    if let Some(found) = find_inline_svg_box(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn svg_root_transform_attribute_is_neutralized_in_serialized_markup() {
  let html = r#"
    <html>
      <body>
        <svg transform="translate(10 0)" width="10" height="10">
          <rect width="10" height="10" />
        </svg>
      </body>
    </html>
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &StyleSheet::new());
  let tree = generate_box_tree(&styled).expect("box tree");

  let svg_box = find_inline_svg_box(&tree.root).expect("inline svg box");
  assert!(
    svg_box.style.has_transform(),
    "svg transform attribute should participate in cascade and be applied externally"
  );

  let BoxType::Replaced(replaced) = &svg_box.box_type else {
    panic!("expected replaced box");
  };
  let ReplacedType::Svg { content } = &replaced.replaced_type else {
    panic!("expected svg replaced type");
  };

  let doc = roxmltree::Document::parse(&content.svg).expect("parse serialized svg");
  let root = doc.root_element();
  assert_eq!(root.tag_name().name(), "svg");
  assert!(
    root.attribute("transform").is_none(),
    "serialized root svg must not include transform attribute"
  );
  let style = root.attribute("style").expect("root style attribute");
  assert!(
    style.contains("transform: none"),
    "serialized root svg must neutralize transform via style attribute: {style}"
  );
}

