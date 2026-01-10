use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

/// Collapsible whitespace runs that are the *only* in-flow content in a block formatting context
/// must not generate empty line boxes.
///
/// An empty line box advances the block cursor and can push subsequent floats down, which is
/// particularly visible in float-based button groups (e.g. Bootstrap navbars).
#[test]
fn collapsible_whitespace_only_buffer_does_not_push_floats_down() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let whitespace_style = Arc::new(ComputedStyle::default());
  let whitespace_text = BoxNode::new_text(whitespace_style.clone(), "\n    ".to_string());
  let whitespace = BoxNode::new_anonymous_inline(whitespace_style.clone(), vec![whitespace_text]);

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(10.0));
  let float_style = Arc::new(float_style);

  let mut float_a = BoxNode::new_inline_block(float_style.clone(), FormattingContextType::Block, vec![]);
  float_a.id = 1;
  let mut float_b = BoxNode::new_inline_block(float_style, FormattingContextType::Block, vec![]);
  float_b.id = 2;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![whitespace.clone(), float_a, whitespace.clone(), float_b, whitespace],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let float_a_frag = find_fragment_by_box_id(&fragment, 1).expect("float A fragment should exist");
  let float_b_frag = find_fragment_by_box_id(&fragment, 2).expect("float B fragment should exist");
  assert!(
    float_a_frag.bounds.y().abs() < 0.01,
    "expected first float to start at y=0, got y={:.2}",
    float_a_frag.bounds.y()
  );
  assert!(
    float_b_frag.bounds.y().abs() < 0.01,
    "expected second float to start at y=0, got y={:.2}",
    float_b_frag.bounds.y()
  );
  assert!(
    float_a_frag.bounds.x().abs() < 0.01,
    "expected first float to start at x=0, got x={:.2}",
    float_a_frag.bounds.x()
  );
  assert!(
    (float_b_frag.bounds.x() - 50.0).abs() < 0.01,
    "expected second float to be placed next to the first, got x={:.2}",
    float_b_frag.bounds.x()
  );
}
