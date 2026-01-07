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
fn container_query_in_shadow_root_selects_query_container_via_flat_tree_ancestors() {
  let html = r#"
    <div id=host>
      <template shadowroot=open>
        <style>
          #container { width: 200px; container-type: inline-size; }
          #shadowed { color: rgb(0 128 0); }
          ::slotted(span) { color: rgb(0 0 255); }
          @container (min-width: 150px) {
            #shadowed { color: rgb(255 0 0); }
            ::slotted(span) { color: rgb(255 0 0); }
          }
        </style>
        <div id=container>
          <span id=shadowed>Shadow</span>
          <slot></slot>
        </div>
      </template>
      <span id=slotted>Hi</span>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let shadowed = find_by_id(&styled, "shadowed").expect("shadow span");
  assert_eq!(shadowed.styles.color, Rgba::rgb(255, 0, 0));
  let target = find_by_id(&styled, "slotted").expect("slotted span");
  assert_eq!(target.styles.color, Rgba::rgb(255, 0, 0));
}
