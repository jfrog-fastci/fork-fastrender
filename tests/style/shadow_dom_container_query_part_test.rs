use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaType;

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
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn container_query_can_match_query_container_in_shadow_tree_for_part_elements() {
  let html = r#"
    <style>
      x-host::part(label) { color: rgb(0 0 255); }
      @container (min-width: 150px) {
        x-host::part(label) { color: rgb(255 0 0); }
      }
    </style>
    <x-host>
      <template shadowroot=open>
        <style>
          #container { width: 200px; container-type: inline-size; }
        </style>
        <div id=container>
          <span id=parted part=label>Hi</span>
        </div>
      </template>
    </x-host>
  "#;

  let styled = styled_tree_for(html);
  let part = find_by_id(&styled, "parted").expect("part element");
  assert_eq!(part.styles.color, Rgba::rgb(255, 0, 0));
}

