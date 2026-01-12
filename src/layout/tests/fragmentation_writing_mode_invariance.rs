use std::sync::Arc;

use fastrender::layout::fragmentation::{
  fragment_tree, resolve_fragmentation_boundaries_with_context, FragmentationContext,
  FragmentationOptions,
};
use fastrender::style::types::{BreakBetween, WritingMode};
use fastrender::{ComputedStyle, FragmentContent, FragmentNode, Rect};

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  let mut out = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Block { box_id: Some(b) } = node.content {
      if b == id {
        out.push(node);
      }
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  out
}

fn assert_f32_vec_close(a: &[f32], b: &[f32]) {
  assert_eq!(a.len(), b.len(), "length mismatch: {a:?} vs {b:?}");
  for (idx, (av, bv)) in a.iter().copied().zip(b.iter().copied()).enumerate() {
    assert!(
      (av - bv).abs() < 0.01,
      "value mismatch at index {idx}: {av} vs {bv} (a={a:?}, b={b:?})"
    );
  }
}

fn assert_rect_close(a: &Rect, b: &Rect) {
  let eps = 0.01;
  assert!(
    (a.x() - b.x()).abs() < eps
      && (a.y() - b.y()).abs() < eps
      && (a.width() - b.width()).abs() < eps
      && (a.height() - b.height()).abs() < eps,
    "rect mismatch: {:?} vs {:?}",
    a,
    b
  );
}

fn build_tree(descendant_writing_mode: WritingMode) -> FragmentNode {
  let mut styled_child_style = ComputedStyle::default();
  styled_child_style.break_after = BreakBetween::Page;
  styled_child_style.writing_mode = descendant_writing_mode;
  let styled_child_style = Arc::new(styled_child_style);

  let mut styled_child =
    FragmentNode::new_block_with_id(Rect::from_xywh(100.0, 0.0, 20.0, 70.0), 1, vec![]);
  styled_child.style = Some(styled_child_style);

  let plain_child =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 70.0, 20.0, 70.0), 2, vec![]);

  FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 140.0),
    vec![styled_child, plain_child],
  )
}

#[test]
fn fragmentation_axis_is_invariant_to_descendant_writing_mode() {
  let fragmentainer_size = 60.0;
  let options = FragmentationOptions::new(fragmentainer_size);

  let horizontal = build_tree(WritingMode::HorizontalTb);
  let vertical_rl = build_tree(WritingMode::VerticalRl);

  let boundaries_horizontal = resolve_fragmentation_boundaries_with_context(
    &horizontal,
    fragmentainer_size,
    FragmentationContext::Page,
  )
  .expect("boundary resolution succeeds");
  let boundaries_vertical_rl = resolve_fragmentation_boundaries_with_context(
    &vertical_rl,
    fragmentainer_size,
    FragmentationContext::Page,
  )
  .expect("boundary resolution succeeds");
  assert_f32_vec_close(&boundaries_horizontal, &boundaries_vertical_rl);

  let fragments_horizontal = fragment_tree(&horizontal, &options).expect("fragmentation succeeds");
  let fragments_vertical_rl =
    fragment_tree(&vertical_rl, &options).expect("fragmentation succeeds");
  assert_eq!(fragments_horizontal.len(), fragments_vertical_rl.len());

  for (idx, (frag_h, frag_v)) in fragments_horizontal
    .iter()
    .zip(fragments_vertical_rl.iter())
    .enumerate()
  {
    let h_nodes = fragments_with_id(frag_h, 1);
    let v_nodes = fragments_with_id(frag_v, 1);
    assert_eq!(
      h_nodes.len(),
      v_nodes.len(),
      "id=1 fragment count mismatch at fragment index {idx}"
    );
    for (h, v) in h_nodes.iter().zip(v_nodes.iter()) {
      assert_rect_close(&h.bounds, &v.bounds);
    }
  }
}
