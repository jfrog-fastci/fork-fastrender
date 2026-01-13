use crate::geometry::{Point, Rect};
use crate::interaction::dom_index::DomIndex;
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxNode;
use crate::PreparedDocument;

pub fn element_border_rect_by_id(prepared: &PreparedDocument, element_id: &str) -> Option<Rect> {
  let scroll_state = prepared.default_scroll_state();
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}

pub fn element_border_rect_by_id_with_scroll_state(
  prepared: &PreparedDocument,
  element_id: &str,
  scroll_state: &ScrollState,
) -> Option<Rect> {
  let mut dom = prepared.dom().clone();
  let dom_index = DomIndex::build(&mut dom);
  let node_id = *dom_index.id_by_element_id.get(element_id)?;

  // Convert the fragment tree into paint-time geometry coordinates (element scroll offsets +
  // sticky adjustments). The returned tree is still in page coordinates; convert it to
  // viewport-local space at the end by subtracting the viewport scroll offset.
  let geometry_tree = prepared.fragment_tree_for_geometry(scroll_state);

  let mut out: Option<Rect> = None;
  // Match `BoxTree::assign_implicit_anchor_box_ids` traversal order.
  let mut stack: Vec<&BoxNode> = vec![&prepared.box_tree().root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(node_id) {
      if let Some(bounds) = crate::interaction::fragment_geometry::absolute_bounds_for_box_id(
        &geometry_tree,
        node.id,
      ) {
        out = Some(match out {
          Some(existing) => existing.union(bounds),
          None => bounds,
        });
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let rect = out?;
  let viewport_scroll = scroll_state.viewport;
  Some(rect.translate(Point::new(-viewport_scroll.x, -viewport_scroll.y)))
}

pub fn element_border_rect_by_id_with_viewport_scroll(
  prepared: &PreparedDocument,
  element_id: &str,
  viewport_scroll: Point,
) -> Option<Rect> {
  let scroll_state = ScrollState::with_viewport(viewport_scroll);
  element_border_rect_by_id_with_scroll_state(prepared, element_id, &scroll_state)
}
