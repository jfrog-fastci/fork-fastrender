use fastrender::css::parser::extract_css;
use fastrender::dom;
use fastrender::style::display::Display;
use fastrender::style::media::MediaContext;
use fastrender::style::{cascade::apply_styles_with_media, ComputedStyle};
use fastrender::tree::box_generation::generate_box_tree_with_anonymous_fixup;
use fastrender::tree::box_tree::{AnonymousType, BoxNode, BoxType};
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{LayoutConfig, LayoutEngine, Size};

fn find_first_flex_box<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
  if node.style.display == Display::Flex {
    return Some(node);
  }
  node.children.iter().find_map(find_first_flex_box)
}

fn find_first_flex_fragment<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style: &std::sync::Arc<ComputedStyle>| style.display == Display::Flex)
  {
    return Some(node);
  }
  node.children.iter().find_map(find_first_flex_fragment)
}

#[test]
fn flex_items_blockify_inline_replaced_children_before_anonymous_fixup() {
  // Regression test for flex/grid item "blockification". Prior to blockification,
  // an inline-level replaced element inside a flex container could be wrapped in an
  // anonymous block box, inflating the flex line cross-size due to line-height.
  //
  // This mimics the Newsweek breaking bar: the flex container inherits a large
  // line-height from the body, but a sibling item has a smaller line-height.
  let html = r#"
  <style>
    body { margin: 0; font-size: 14px; line-height: 1.766; }
    #bar { display: flex; padding-top: 8px; padding-bottom: 8px; }
    #text { line-height: 1.5; }
  </style>
  <div id="bar">
    <div id="text">Breaking</div>
    <svg width="15" height="14" viewBox="0 0 15 14">
      <rect width="15" height="14" fill="black"></rect>
    </svg>
  </div>
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = extract_css(&dom).expect("extract css");
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let flex_box = find_first_flex_box(&box_tree.root).expect("flex container box");
  assert!(
    flex_box
      .children
      .iter()
      .any(|child| matches!(child.box_type, BoxType::Replaced(_))),
    "expected inline SVG to generate a replaced box"
  );
  assert!(
    !flex_box.children.iter().any(|child| matches!(
      &child.box_type,
      BoxType::Anonymous(anon) if matches!(anon.anonymous_type, AnonymousType::Block)
    )),
    "expected flex items to be blockified (no anonymous block wrappers)"
  );

  let engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(800.0, 600.0)));
  let fragments = engine.layout_tree(&box_tree).expect("layout");
  let flex_fragment = find_first_flex_fragment(&fragments.root).expect("flex fragment");

  // The tallest flex item is the text line box: 14px * 1.5 = 21px. With 8px padding
  // on top and bottom, the flex container's height should be 21 + 16 = 37px.
  let height = flex_fragment.bounds.height();
  assert!(
    (height - 37.0).abs() < 0.1,
    "expected flex container height to be 37px after blockification, got {height}"
  );
}

