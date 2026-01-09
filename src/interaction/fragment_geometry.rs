use crate::geometry::Point;
use crate::geometry::Rect;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;

pub fn absolute_bounds_for_box_id(tree: &FragmentTree, box_id: usize) -> Option<Rect> {
  struct Frame<'a> {
    node: &'a FragmentNode,
    parent_offset: Point,
  }

  let mut bounds: Option<Rect> = None;

  let mut stack: Vec<Frame<'_>> = Vec::new();
  for root in tree.additional_fragments.iter().rev() {
    stack.push(Frame {
      node: root,
      parent_offset: Point::ZERO,
    });
  }
  stack.push(Frame {
    node: &tree.root,
    parent_offset: Point::ZERO,
  });

  while let Some(frame) = stack.pop() {
    let absolute_bounds = frame.node.bounds.translate(frame.parent_offset);
    if frame.node.box_id() == Some(box_id) {
      bounds = Some(match bounds {
        Some(existing) => existing.union(absolute_bounds),
        None => absolute_bounds,
      });
    }

    let child_parent_offset = absolute_bounds.origin;
    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_offset: child_parent_offset,
      });
    }
  }

  bounds
}
