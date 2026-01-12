use crate::api::FastRender;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::style::types::{AlignItems, AnchorSide, InsetValue};
use crate::style::values::Length;

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
fn anchor_function_parses_spec_side_keywords() {
  let html = r#"
    <style>
      #target {
        left: anchor(start);
        right: anchor(end, 2px);
        top: anchor(self-start);
        bottom: anchor(self-end);
      }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  match &target.styles.left {
    InsetValue::Anchor(anchor) => {
      assert_eq!(anchor.side, AnchorSide::Start);
      assert!(anchor.name.is_none());
      assert!(anchor.fallback.is_none());
    }
    other => panic!("expected left: anchor(...), got {other:?}"),
  }

  match &target.styles.right {
    InsetValue::Anchor(anchor) => {
      assert_eq!(anchor.side, AnchorSide::End);
      assert_eq!(anchor.fallback, Some(Length::px(2.0)));
    }
    other => panic!("expected right: anchor(...), got {other:?}"),
  }

  match &target.styles.top {
    InsetValue::Anchor(anchor) => {
      assert_eq!(anchor.side, AnchorSide::SelfStart);
    }
    other => panic!("expected top: anchor(...), got {other:?}"),
  }

  match &target.styles.bottom {
    InsetValue::Anchor(anchor) => {
      assert_eq!(anchor.side, AnchorSide::SelfEnd);
    }
    other => panic!("expected bottom: anchor(...), got {other:?}"),
  }
}

#[test]
fn anchor_center_parses_for_alignment_properties() {
  let html = r#"
    <style>
      #target {
        align-self: anchor-center;
        justify-self: anchor-center;
        align-items: anchor-center;
        justify-items: anchor-center;
      }
    </style>
    <div id="target"></div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  assert_eq!(target.styles.align_self, Some(AlignItems::AnchorCenter));
  assert_eq!(target.styles.justify_self, Some(AlignItems::AnchorCenter));
  assert_eq!(target.styles.align_items, AlignItems::AnchorCenter);
  assert_eq!(target.styles.justify_items, AlignItems::AnchorCenter);
}
