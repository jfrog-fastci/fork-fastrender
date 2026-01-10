use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_float_fragment<'a>(node: &'a FragmentNode, width: f32, height: f32) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && (node.bounds.width() - width).abs() < 0.01
    && (node.bounds.height() - height).abs() < 0.01
  {
    return Some(node);
  }
  for child in &node.children {
    if let Some(found) = find_float_fragment(child, width, height) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_level_float_after_text_floats_up_to_line_top() {
  // Floats are taken out of flow and are positioned as high as possible (CSS 2.1 §9.5.1). When a
  // float appears after inline content in the source, browsers still allow it to float up next to
  // that content on the current line box rather than forcing it below the already-laid-out line.
  //
  // Regression: block layout previously flushed buffered inline content before placing a float,
  // which advanced the block cursor and incorrectly forced inline-level floats onto the next line.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let text_style = Arc::new(ComputedStyle::default());
  let text_before = BoxNode::new_text(text_style.clone(), "before".to_string());
  let text_after = BoxNode::new_text(text_style, "after".to_string());

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(10.0));
  let float_node = BoxNode::new_inline_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![text_before, float_node, text_after],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let float_frag =
    find_float_fragment(&fragment, 50.0, 10.0).expect("expected to find float fragment");
  assert!(
    float_frag.bounds.y().abs() < 0.5,
    "expected float to be placed at the top of the first line, got y={:.2}",
    float_frag.bounds.y()
  );
}
