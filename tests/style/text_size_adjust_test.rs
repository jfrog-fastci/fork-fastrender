use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::style::types::TextSizeAdjust;

fn styled_tree_for(html: &str) -> StyledNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
    .styled_tree
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn text_size_adjust_parses_percent_length_token() {
  let html = r#"
    <style>
      #target { text-size-adjust: 125%; }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");
  assert!(
    matches!(target.styles.text_size_adjust, TextSizeAdjust::Percentage(p) if (p - 125.0).abs() < f32::EPSILON)
  );
}

