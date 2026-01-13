use crate::api::FastRender;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::style::types::HangingPunctuation;

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
fn hanging_punctuation_parses_and_inherits() {
  let html = r#"
    <style>
      #parent { hanging-punctuation: first allow-end; }
      #child-inherit { hanging-punctuation: inherit; }
      #child-initial { hanging-punctuation: initial; }
      #child-invalid { hanging-punctuation: first allow-end force-end; }
      #order-a { hanging-punctuation: first allow-end; }
      #order-b { hanging-punctuation: allow-end first; }
    </style>
    <div id="parent">
      <div id="child-inherit"></div>
      <div id="child-initial"></div>
      <div id="child-invalid"></div>
    </div>
    <div id="order-a"></div>
    <div id="order-b"></div>
  "#;

  let styled = styled_tree_for(html);
  let parent = find_by_id(&styled, "parent").expect("parent element");
  let child_inherit = find_by_id(&styled, "child-inherit").expect("inherit child");
  let child_initial = find_by_id(&styled, "child-initial").expect("initial child");
  let child_invalid = find_by_id(&styled, "child-invalid").expect("invalid child");
  let order_a = find_by_id(&styled, "order-a").expect("order a");
  let order_b = find_by_id(&styled, "order-b").expect("order b");

  assert!(
    parent.styles.hanging_punctuation.has_first(),
    "expected parent to have `first`"
  );
  assert!(
    parent.styles.hanging_punctuation.has_allow_end(),
    "expected parent to have `allow-end`"
  );
  assert!(
    !parent.styles.hanging_punctuation.has_force_end(),
    "expected parent to not have `force-end`"
  );

  // Inherited property.
  assert_eq!(
    child_inherit.styles.hanging_punctuation,
    parent.styles.hanging_punctuation
  );

  // Initial value.
  assert_eq!(child_initial.styles.hanging_punctuation, HangingPunctuation::NONE);

  // Invalid declaration is ignored, so the property inherits from the parent.
  assert_eq!(
    child_invalid.styles.hanging_punctuation,
    parent.styles.hanging_punctuation
  );

  // Canonical ordering is per-grammar; parsing should accept any order.
  assert_eq!(
    order_a.styles.hanging_punctuation,
    order_b.styles.hanging_punctuation
  );
}

