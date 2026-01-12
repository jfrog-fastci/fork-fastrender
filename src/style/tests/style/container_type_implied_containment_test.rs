use crate::api::FastRender;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;

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
fn container_type_folds_implied_containment_into_computed_style() {
  let html = r#"
    <style>
      #size { container-type: size; }
      #inline { container-type: inline-size; }
      #scroll { container-type: scroll-state; }
      #size_scroll { container-type: size scroll-state; }
      #inline_scroll { container-type: inline-size scroll-state; }
      #combo { contain: layout; container-type: inline-size; }
      #normal { container-type: normal; }
    </style>
    <div id="size"></div>
    <div id="inline"></div>
    <div id="scroll"></div>
    <div id="size_scroll"></div>
    <div id="inline_scroll"></div>
    <div id="combo"></div>
    <div id="normal"></div>
  "#;

  let styled = styled_tree_for(html);

  let size = find_by_id(&styled, "size").expect("size element");
  assert!(size.styles.containment.size);
  assert!(!size.styles.containment.inline_size);
  assert!(size.styles.containment.style);
  assert!(!size.styles.containment.layout);
  assert!(!size.styles.containment.paint);

  let inline = find_by_id(&styled, "inline").expect("inline element");
  assert!(!inline.styles.containment.size);
  assert!(inline.styles.containment.inline_size);
  assert!(inline.styles.containment.style);
  assert!(!inline.styles.containment.layout);
  assert!(!inline.styles.containment.paint);

  let scroll = find_by_id(&styled, "scroll").expect("scroll element");
  assert_eq!(
    scroll.styles.containment,
    crate::style::types::Containment::none()
  );

  let size_scroll = find_by_id(&styled, "size_scroll").expect("size_scroll element");
  assert!(size_scroll.styles.containment.size);
  assert!(!size_scroll.styles.containment.inline_size);
  assert!(size_scroll.styles.containment.style);
  assert!(!size_scroll.styles.containment.layout);
  assert!(!size_scroll.styles.containment.paint);

  let inline_scroll = find_by_id(&styled, "inline_scroll").expect("inline_scroll element");
  assert!(!inline_scroll.styles.containment.size);
  assert!(inline_scroll.styles.containment.inline_size);
  assert!(inline_scroll.styles.containment.style);
  assert!(!inline_scroll.styles.containment.layout);
  assert!(!inline_scroll.styles.containment.paint);

  let combo = find_by_id(&styled, "combo").expect("combo element");
  assert!(combo.styles.containment.layout);
  assert!(combo.styles.containment.style);
  assert!(combo.styles.containment.inline_size);
  assert!(!combo.styles.containment.size);
  assert!(!combo.styles.containment.paint);

  let normal = find_by_id(&styled, "normal").expect("normal element");
  assert_eq!(
    normal.styles.containment,
    crate::style::types::Containment::none()
  );
}
