use crate::layout::axis::FragmentAxes;
use crate::style::content::{RunningElementSelect, RunningElementValues};
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::collections::HashMap;
use std::sync::Arc;

const EPSILON: f32 = 0.01;

/// A running element occurrence extracted from the laid-out fragment tree.
#[derive(Debug, Clone)]
pub struct RunningElementEvent {
  /// Absolute block-axis position of the running element.
  pub abs_block: f32,
  /// Running name from `position: running(<name>)`.
  pub name: String,
  /// Snapshot of the laid-out element subtree.
  pub snapshot: FragmentNode,
}

/// Global state for running elements across pages.
#[derive(Debug, Default, Clone)]
pub struct RunningElementState {
  /// First occurrence in the document for each name.
  pub first: HashMap<String, FragmentNode>,
  /// Last occurrence seen so far (up to the start of the current page).
  pub last: HashMap<String, FragmentNode>,
}

/// Clears `running_position` flags from a cloned box subtree.
///
/// Text and anonymous boxes in the box tree can share the same computed style as their parent
/// element. When a `position: running(<name>)` element is snapshotted for margin boxes, those
/// descendants must not be treated as nested running elements during the snapshot layout pass.
pub(crate) fn clear_running_position_in_box_tree(node: &mut BoxNode) {
  // Use an explicit stack to avoid recursion on pathological trees.
  let mut stack: Vec<*mut BoxNode> = vec![node as *mut BoxNode];
  while let Some(node_ptr) = stack.pop() {
    // SAFETY: The stack only contains pointers to nodes owned by `node`. We never move those nodes
    // during the traversal, and each pointer is used once to mutate the node before pushing its
    // children.
    unsafe {
      let node = &mut *node_ptr;
      if node.style.running_position.is_some() {
        let mut owned = node.style.as_ref().clone();
        owned.running_position = None;
        node.style = Arc::new(owned);
      }
      if let Some(body) = node.footnote_body.as_deref_mut() {
        stack.push(body as *mut BoxNode);
      }
      for child in node.children.iter_mut() {
        stack.push(child as *mut BoxNode);
      }
    }
  }
}

/// Collect all running element occurrences from the laid-out fragment tree.
pub fn collect_running_element_events(
  root: &FragmentNode,
  axes: FragmentAxes,
) -> Vec<RunningElementEvent> {
  let mut events = Vec::new();
  collect_running_element_occurrences(root, 0.0, axes.block_size(&root.logical_bounds()), axes, &mut events);
  events.sort_by(|a, b| {
    a.abs_block
      .partial_cmp(&b.abs_block)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  events
}

/// Collect running element events from a page subtree.
///
/// Pagination translates the clipped page content subtree into the page box after clipping, so the
/// root fragment is generally offset from the page content origin. Margin box selection, however,
/// treats the page content block-start edge as position 0. This helper shifts the coordinate space
/// so that `abs_block == 0` corresponds to the page content start, ensuring `element(name, start)`
/// behaves consistently across writing modes (including reversed block progression).
pub fn collect_running_element_events_for_page(
  root: &FragmentNode,
  axes: FragmentAxes,
) -> (Vec<RunningElementEvent>, f32) {
  let bounds = root.logical_bounds();
  let block_size = axes.block_size(&bounds);
  let root_start = axes.block_start(&bounds, block_size);
  let mut events = Vec::new();
  collect_running_element_occurrences(root, -root_start, block_size, axes, &mut events);
  events.sort_by(|a, b| {
    a.abs_block
      .partial_cmp(&b.abs_block)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  (events, block_size)
}

/// Computes running element values for a single page subtree.
///
/// The returned map contains [`RunningElementValues`] keyed by running element name. The
/// `start` field is initialized from [`RunningElementState::last`] (the carried value from the
/// previous page), and is replaced with the first occurrence in this page when that occurrence is
/// positioned at the page start boundary (within `EPSILON`).
pub fn running_elements_for_page_fragment(
  root: &FragmentNode,
  axes: FragmentAxes,
  state: &mut RunningElementState,
) -> HashMap<String, RunningElementValues> {
  let (events, block_size) = collect_running_element_events_for_page(root, axes);
  let mut idx = 0usize;
  running_elements_for_page(&events, &mut idx, state, 0.0, block_size)
}

/// Compute running element values for a page range.
///
/// Events are consumed in-order as pages advance to avoid re-scanning the fragment tree.
pub fn running_elements_for_page(
  events: &[RunningElementEvent],
  idx: &mut usize,
  state: &mut RunningElementState,
  start: f32,
  end: f32,
) -> HashMap<String, RunningElementValues> {
  let boundary = start - EPSILON;
  while *idx < events.len() && events[*idx].abs_block < boundary {
    let event = &events[*idx];
    state
      .first
      .entry(event.name.clone())
      .or_insert_with(|| event.snapshot.clone());
    state
      .last
      .insert(event.name.clone(), event.snapshot.clone());
    *idx += 1;
  }

  let mut values: HashMap<String, RunningElementValues> = HashMap::new();
  for (name, last) in state.last.iter() {
    values.insert(
      name.clone(),
      RunningElementValues {
        start: Some(last.clone()),
        first: None,
        last: None,
      },
    );
  }

  while *idx < events.len() && events[*idx].abs_block < end {
    let event = &events[*idx];
    let entry = values
      .entry(event.name.clone())
      .or_insert_with(|| RunningElementValues {
        start: state.last.get(&event.name).cloned(),
        first: None,
        last: None,
      });
    if entry.first.is_none() {
      if (event.abs_block - start).abs() < EPSILON {
        entry.start = Some(event.snapshot.clone());
      }
      entry.first = Some(event.snapshot.clone());
    }
    entry.last = Some(event.snapshot.clone());
    state
      .first
      .entry(event.name.clone())
      .or_insert_with(|| event.snapshot.clone());
    state
      .last
      .insert(event.name.clone(), event.snapshot.clone());
    *idx += 1;
  }

  values
}

/// Resolves a running element snapshot for `element()` in @page margin boxes.
///
/// This matches the engine's [`ContentContext`](crate::style::content::ContentContext) semantics:
///
/// - `first`: first occurrence within the page, falling back to the carried `start` value.
/// - `start`: carried value at the page start, replaced by the page's first occurrence when that
///   occurrence begins exactly at the page start boundary.
/// - `last`: last occurrence within the page, falling back to the carried `start` value.
/// - `first-except`: resolves to `None` on pages where an occurrence exists; otherwise behaves like
///   `first` (falling back to the carried `start` value).
pub fn select_running_element(
  ident: &str,
  select: RunningElementSelect,
  page_values: &HashMap<String, RunningElementValues>,
) -> Option<FragmentNode> {
  let page = page_values.get(ident);
  match select {
    RunningElementSelect::First => page.and_then(|v| v.first.clone().or_else(|| v.start.clone())),
    RunningElementSelect::Start => page.and_then(|v| v.start.clone()),
    RunningElementSelect::Last => page.and_then(|v| v.last.clone().or_else(|| v.start.clone())),
    RunningElementSelect::FirstExcept => {
      if page.is_some_and(|v| v.first.is_some()) {
        None
      } else {
        page.and_then(|v| v.first.clone().or_else(|| v.start.clone()))
      }
    }
  }
}

fn collect_running_element_occurrences(
  node: &FragmentNode,
  abs_block_start: f32,
  parent_block_size: f32,
  axes: FragmentAxes,
  out: &mut Vec<RunningElementEvent>,
) {
  let logical_bounds = node.logical_bounds();
  let node_abs_block = axes.abs_block_start(&logical_bounds, abs_block_start, parent_block_size);
  let node_block_size = axes.block_size(&logical_bounds);

  match &node.content {
    FragmentContent::RunningAnchor { name, snapshot } => {
      let mut cleaned = (**snapshot).clone();
      clean_running_snapshot(&mut cleaned);
      out.push(RunningElementEvent {
        abs_block: node_abs_block,
        name: name.to_string(),
        snapshot: cleaned,
      });
    }
    _ if node.content.is_block() || node.content.is_inline() || node.content.is_replaced() => {
      if let Some(name) = node
        .style
        .as_deref()
        .and_then(|style| style.running_position.as_ref())
      {
        let mut clone = node.clone();
        clean_running_snapshot(&mut clone);
        out.push(RunningElementEvent {
          abs_block: node_abs_block,
          name: name.clone(),
          snapshot: clone,
        });
      }
    }
    _ => {}
  }

  for child in node.children() {
    collect_running_element_occurrences(child, node_abs_block, node_block_size, axes, out);
  }
}

fn clean_running_snapshot(node: &mut FragmentNode) {
  strip_running_anchor_fragments(node);
  clear_running_position(node);
  let offset = crate::geometry::Point::new(-node.bounds.x(), -node.bounds.y());
  node.translate_root_in_place(offset);
  if let Some(logical) = node.logical_override {
    node.logical_override = Some(crate::geometry::Rect::from_xywh(
      0.0,
      0.0,
      logical.width(),
      logical.height(),
    ));
  }
}

fn strip_running_anchor_fragments(node: &mut FragmentNode) {
  let children = node.children_mut();
  let mut kept: Vec<FragmentNode> = Vec::with_capacity(children.len());
  for mut child in children.drain(..) {
    if matches!(child.content, FragmentContent::RunningAnchor { .. }) {
      continue;
    }
    strip_running_anchor_fragments(&mut child);
    kept.push(child);
  }
  *children = kept.into();
}

fn clear_running_position(node: &mut FragmentNode) {
  if let Some(style) = node.style.as_deref() {
    if style.running_position.is_some() {
      let mut owned = style.clone();
      owned.running_position = None;
      node.style = Some(Arc::new(owned));
    }
  }
  for child in node.children_mut().iter_mut() {
    clear_running_position(child);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::style::ComputedStyle;

  #[test]
  fn clear_running_position_visits_footnote_body() {
    let mut style = ComputedStyle::default();
    style.running_position = Some("header".to_string());
    let style = Arc::new(style);

    let body = BoxNode::new_inline(style.clone(), Vec::new());
    let mut call = BoxNode::new_inline(style.clone(), Vec::new());
    call.footnote_body = Some(Box::new(body));

    clear_running_position_in_box_tree(&mut call);
    assert!(call.style.running_position.is_none());
    assert!(
      call
        .footnote_body
        .as_ref()
        .is_some_and(|body| body.style.running_position.is_none()),
      "expected running_position to be cleared inside footnote body"
    );
  }

  #[test]
  fn clean_running_snapshot_strips_and_normalizes() {
    let mut root_style = ComputedStyle::default();
    root_style.running_position = Some("root".into());
    let root_style = Arc::new(root_style);

    let mut child_style = ComputedStyle::default();
    child_style.running_position = Some("child".into());
    let child_style = Arc::new(child_style);

    let anchor_snapshot = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 5.0, 5.0), Vec::new());
    let anchor = FragmentNode::new_running_anchor(
      Rect::from_xywh(1.0, 1.0, 2.0, 2.0),
      "anchor".to_string(),
      anchor_snapshot,
    );
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(4.0, 6.0, 10.0, 10.0),
      Vec::new(),
      child_style,
    );

    let mut root = FragmentNode::new_block_styled(
      Rect::from_xywh(10.0, 20.0, 30.0, 30.0),
      vec![anchor, child],
      root_style,
    );

    clean_running_snapshot(&mut root);

    assert!(!root
      .iter_fragments()
      .any(|frag| matches!(frag.content, FragmentContent::RunningAnchor { .. })));

    assert!(root.iter_fragments().all(|frag| {
      frag
        .style
        .as_deref()
        .map_or(true, |style| style.running_position.is_none())
    }));

    assert!(root.bounds.x().abs() < EPSILON);
    assert!(root.bounds.y().abs() < EPSILON);

    let logical = root.logical_bounds();
    assert!(logical.x().abs() < EPSILON);
    assert!(logical.y().abs() < EPSILON);
  }
}
