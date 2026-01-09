use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::style::types::AnchorSizeAxis;
use fastrender::style::values::Length;

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
fn anchor_size_parses_axis_optional_name_and_fallback() {
  let html = r#"
    <style>
      #target {
        width: anchor-size(width);
        height: anchor-size(--a block-size, 10px);
        min-width: anchor-size(block-size --a, -5px);
      }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  let width = target.styles.width_anchor_size.as_ref().expect("width anchor-size");
  assert_eq!(width.axis, AnchorSizeAxis::Width);
  assert!(width.name.is_none());
  assert!(width.fallback.is_none());

  let height = target
    .styles
    .height_anchor_size
    .as_ref()
    .expect("height anchor-size");
  assert_eq!(height.axis, AnchorSizeAxis::BlockSize);
  assert_eq!(height.name.as_deref(), Some("--a"));
  assert_eq!(height.fallback, Some(Length::px(10.0)));

  let min_width = target
    .styles
    .min_width_anchor_size
    .as_ref()
    .expect("min-width anchor-size");
  assert_eq!(min_width.axis, AnchorSizeAxis::BlockSize);
  assert_eq!(min_width.name.as_deref(), Some("--a"));
  assert_eq!(
    min_width.fallback,
    Some(Length::px(0.0)),
    "min-width fallback should clamp negative lengths to 0"
  );
}

#[test]
fn anchor_size_rejects_invalid_axis_and_does_not_override_previous_value() {
  let html = r#"
    <style>
      #target {
        width: 100px;
        width: anchor-size(foo);
        height: 40px;
        height: calc(anchor-size(width));
      }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  assert_eq!(target.styles.width, Some(Length::px(100.0)));
  assert!(target.styles.width_anchor_size.is_none());

  assert_eq!(target.styles.height, Some(Length::px(40.0)));
  assert!(target.styles.height_anchor_size.is_none());
}

