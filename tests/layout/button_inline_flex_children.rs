use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_svg_replaced_fragment<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  match &node.content {
    FragmentContent::Replaced {
      replaced_type: ReplacedType::Svg { .. },
      ..
    } => Some(node),
    _ => node.children.iter().find_map(find_svg_replaced_fragment),
  }
}

fn find_text_fragment_containing<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  match &node.content {
    FragmentContent::Text { text, .. } if text.contains(needle) => Some(node),
    _ => node
      .children
      .iter()
      .find_map(|child| find_text_fragment_containing(child, needle)),
  }
}

#[test]
fn button_inline_flex_lays_out_svg_and_text_children() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let html = r#"
    <button style="display:inline-flex; align-items:center; gap:4px">
      <svg width="12" height="12" viewBox="0 0 12 12" xmlns="http://www.w3.org/2000/svg">
        <rect width="12" height="12" fill="rgb(255, 0, 0)"/>
      </svg>
      Go
    </button>
  "#;

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 50)
    .expect("layout document");

  let svg = find_svg_replaced_fragment(&fragments.root).expect("expected SVG fragment in button");
  assert!(
    svg.bounds.width() > 0.0 && svg.bounds.height() > 0.0,
    "expected SVG fragment to have non-zero bounds, got {:?}",
    svg.bounds
  );

  let text =
    find_text_fragment_containing(&fragments.root, "Go").expect("expected button text fragment");
  assert!(
    text.bounds.width() > 0.0 && text.bounds.height() > 0.0,
    "expected text fragment to have non-zero bounds, got {:?}",
    text.bounds
  );
}

