use fastrender::dom::DomNodeType;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::{
  BoxNode, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Rect,
};

fn find_styled_node_id_for_dom_id(node: &StyledNode, id_value: &str) -> Option<usize> {
  if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
    if attributes
      .iter()
      .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id_value)
    {
      return Some(node.node_id);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_styled_node_id_for_dom_id(child, id_value) {
      return Some(found);
    }
  }

  None
}

fn find_box_id_for_styled_node_id(node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
    return Some(node.id);
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_id_for_styled_node_id(child, styled_node_id) {
      return Some(found);
    }
  }
  if let Some(footnote_body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_id_for_styled_node_id(footnote_body, styled_node_id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_bounds_for_box_id(node: &FragmentNode, box_id: usize) -> Option<Rect> {
  let matches_box = match &node.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline {
      box_id: Some(id), ..
    }
    | FragmentContent::Text {
      box_id: Some(id), ..
    }
    | FragmentContent::Replaced {
      box_id: Some(id), ..
    } => *id == box_id,
    _ => false,
  };
  if matches_box {
    return Some(node.bounds);
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_bounds_for_box_id(child, box_id) {
      return Some(found);
    }
  }

  None
}

#[test]
fn ua_button_default_padding_sizes_svg_buttons_like_chrome() {
  // Regression test for macrumors.com: without author styles, Chrome's default button styles
  // produce a tight 24x24 SVG icon button with smaller padding than we previously used.
  //
  // Force the SVG to be block-level so the button's content height is exactly 24px (no inline
  // baseline/strut effects), making this a stable assertion of UA padding/border behavior.
  let html = r#"<!doctype html>
    <button id="btn">
      <svg style="display:block" width="24" height="24" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        <rect width="24" height="24" fill="black"></rect>
      </svg>
    </button>"#;

  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let dom = renderer.parse_html(html).expect("dom");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, 200, 100, MediaType::Screen)
    .expect("layout intermediates");

  let styled_id = find_styled_node_id_for_dom_id(&intermediates.styled_tree, "btn")
    .expect("button styled node id");
  let box_id =
    find_box_id_for_styled_node_id(&intermediates.box_tree.root, styled_id).expect("button box id");
  let bounds =
    find_fragment_bounds_for_box_id(&intermediates.fragment_tree.root, box_id).expect("bounds");

  // The UA stylesheet sets:
  // - border: 1px solid
  // - padding: 2px 7px
  // With a 24x24 SVG child that yields:
  //   width = 24 + 7 + 7 + 1 + 1 = 40
  //   height = 24 + 2 + 2 + 1 + 1 = 30
  let eps = 0.5;
  assert!(
    (bounds.width() - 40.0).abs() <= eps,
    "expected SVG button width ~= 40px, got {:?}",
    bounds
  );
  assert!(
    (bounds.height() - 30.0).abs() <= eps,
    "expected SVG button height ~= 30px, got {:?}",
    bounds
  );
}
