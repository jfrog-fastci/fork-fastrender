use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::Length;

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
fn container_query_units_in_inline_style_trigger_container_pass() {
  let html = r#"
    <style>
      .container { width: 400px; height: 80px; background: #eee; }
      .child { height: 20px; background: red; }
    </style>
    <div class="container" style="container-type: inline-size">
      <div id="target" class="child" style="width: 50cqw"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  assert_eq!(target.styles.width, Some(Length::px(200.0)));
}
