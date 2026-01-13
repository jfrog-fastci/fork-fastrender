use super::{Document, DomError, DomResult, LiveRangeId, NodeId, NodeKind};
use std::cmp::Ordering;

/// A DOM boundary point (node, offset) used by Range algorithms.
///
/// Spec: https://dom.spec.whatwg.org/#concept-range-bp
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryPoint {
  pub node: NodeId,
  pub offset: usize,
}

/// Handle to a live [`Range`] stored in a [`Document`].
///
/// `Range` platform objects are GC-managed in JS. `dom2` therefore keys its range state by a stable
/// monotonic [`LiveRangeId`], which is tracked weakly by `LiveMutation` and swept when wrappers are
/// collected.
pub type RangeId = LiveRangeId;

#[derive(Debug, Clone)]
pub(super) struct Range {
  start: BoundaryPoint,
  end: BoundaryPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryPointPosition {
  Before,
  Equal,
  After,
}

fn invert_boundary_point_position(pos: BoundaryPointPosition) -> BoundaryPointPosition {
  match pos {
    BoundaryPointPosition::Before => BoundaryPointPosition::After,
    BoundaryPointPosition::Equal => BoundaryPointPosition::Equal,
    BoundaryPointPosition::After => BoundaryPointPosition::Before,
  }
}

impl Document {
  fn insert_range_state(&mut self, id: RangeId) {
    let start_end = BoundaryPoint {
      node: self.root(),
      offset: 0,
    };
    let prev = self.ranges.insert(
      id,
      Range {
        start: start_end,
        end: start_end,
      },
    );
    debug_assert!(
      prev.is_none(),
      "range id collision: attempted to insert duplicate Range state"
    );
  }

  pub fn create_range(&mut self) -> RangeId {
    let id = self.live_mutation.alloc_live_range_id();
    self.insert_range_state(id);
    id
  }

  pub(crate) fn create_range_for_id(&mut self, id: RangeId) {
    self.insert_range_state(id);
  }

  pub(crate) fn remove_range(&mut self, id: RangeId) {
    self.ranges.remove(&id);
  }

  fn range(&self, range: RangeId) -> DomResult<&Range> {
    self.ranges.get(&range).ok_or(DomError::NotFoundError)
  }

  fn range_mut(&mut self, range: RangeId) -> DomResult<&mut Range> {
    self.ranges.get_mut(&range).ok_or(DomError::NotFoundError)
  }

  pub fn range_start(&self, range: RangeId) -> DomResult<BoundaryPoint> {
    Ok(self.range(range)?.start)
  }

  pub fn range_end(&self, range: RangeId) -> DomResult<BoundaryPoint> {
    Ok(self.range(range)?.end)
  }

  pub fn range_start_container(&self, range: RangeId) -> DomResult<NodeId> {
    Ok(self.range(range)?.start.node)
  }

  pub fn range_start_offset(&self, range: RangeId) -> DomResult<usize> {
    Ok(self.range(range)?.start.offset)
  }

  pub fn range_end_container(&self, range: RangeId) -> DomResult<NodeId> {
    Ok(self.range(range)?.end.node)
  }

  pub fn range_end_offset(&self, range: RangeId) -> DomResult<usize> {
    Ok(self.range(range)?.end.offset)
  }

  pub fn range_set_start(&mut self, range: RangeId, node: NodeId, offset: usize) -> DomResult<()> {
    self.range_set_start_or_end(range, node, offset, /* is_start */ true)
  }

  pub fn range_set_end(&mut self, range: RangeId, node: NodeId, offset: usize) -> DomResult<()> {
    self.range_set_start_or_end(range, node, offset, /* is_start */ false)
  }

  /// ShadowRoot-aware "root of node" helper for DOM Range algorithms.
  ///
  /// `dom2` stores ShadowRoot nodes in the main tree with a `parent` pointer to the host element so
  /// renderer traversal can see them. The DOM Standard's "root" concept instead treats ShadowRoot as
  /// the root of a separate tree (i.e. its parent is null).
  ///
  /// Range boundary point comparison, setStart/setEnd root checks, and live range maintenance must
  /// therefore stop root traversal at ShadowRoot.
  pub fn tree_root_for_range(&self, mut node: NodeId) -> NodeId {
    let mut remaining = self.nodes.len() + 1;
    loop {
      if remaining == 0 {
        // Cycle / corruption guard; fall back to the current node.
        return node;
      }
      remaining -= 1;

      let Some(n) = self.nodes.get(node.index()) else {
        return node;
      };
      match &n.kind {
        NodeKind::ShadowRoot { .. } | NodeKind::Document { .. } => return node,
        _ => {}
      }
      let Some(parent) = n.parent else {
        return node;
      };
      node = parent;
    }
  }

  fn range_parent(&self, node: NodeId) -> Option<NodeId> {
    let node = self.nodes.get(node.index())?;
    if matches!(&node.kind, NodeKind::ShadowRoot { .. }) {
      // Per DOM, ShadowRoot is the root of a separate tree.
      return None;
    }
    node.parent
  }

  fn node_length(&self, node: NodeId) -> DomResult<usize> {
    let node = self.node_checked(node)?;
    Ok(match &node.kind {
      NodeKind::Document { .. }
      | NodeKind::DocumentFragment
      | NodeKind::ShadowRoot { .. }
      | NodeKind::Slot { .. }
      | NodeKind::Element { .. } => node.children.len(),
      NodeKind::Text { content } | NodeKind::Comment { content } => content.encode_utf16().count(),
      NodeKind::ProcessingInstruction { data, .. } => data.encode_utf16().count(),
      NodeKind::Doctype { .. } => 0,
    })
  }

  fn node_index(&self, node: NodeId) -> Option<usize> {
    let parent = self.range_parent(node)?;
    self
      .nodes
      .get(parent.index())?
      .children
      .iter()
      .position(|&c| c == node)
  }

  fn is_ancestor_for_range(&self, ancestor: NodeId, node: NodeId) -> bool {
    if ancestor == node {
      return false;
    }
    let mut current = self.range_parent(node);
    let mut remaining = self.nodes.len() + 1;
    while let Some(id) = current {
      if remaining == 0 {
        return false;
      }
      remaining -= 1;

      if id == ancestor {
        return true;
      }
      current = self.range_parent(id);
    }
    false
  }

  fn compare_tree_order_for_range(&self, a: NodeId, b: NodeId) -> Ordering {
    if a == b {
      return Ordering::Equal;
    }

    debug_assert_eq!(
      self.tree_root_for_range(a),
      self.tree_root_for_range(b),
      "tree order comparisons require nodes in the same tree"
    );

    fn path_to_root(doc: &Document, node: NodeId) -> Vec<NodeId> {
      let mut out: Vec<NodeId> = Vec::new();
      let mut current = Some(node);
      let mut remaining = doc.nodes.len() + 1;
      while let Some(id) = current {
        if remaining == 0 {
          break;
        }
        remaining -= 1;

        out.push(id);
        current = doc.range_parent(id);
      }
      out.reverse();
      out
    }

    let path_a = path_to_root(self, a);
    let path_b = path_to_root(self, b);

    let mut i = 0usize;
    let min_len = path_a.len().min(path_b.len());
    while i < min_len && path_a[i] == path_b[i] {
      i += 1;
    }

    if i == path_a.len() {
      // a is an ancestor of b, so it precedes b in tree order.
      return Ordering::Less;
    }
    if i == path_b.len() {
      return Ordering::Greater;
    }

    let common = path_a[i - 1];
    let child_a = path_a[i];
    let child_b = path_b[i];

    let idx_a = self
      .nodes
      .get(common.index())
      .and_then(|n| n.children.iter().position(|&c| c == child_a));
    let idx_b = self
      .nodes
      .get(common.index())
      .and_then(|n| n.children.iter().position(|&c| c == child_b));

    match (idx_a, idx_b) {
      (Some(a), Some(b)) => a.cmp(&b),
      _ => child_a.index().cmp(&child_b.index()),
    }
  }

  fn boundary_point_position(&self, a: BoundaryPoint, b: BoundaryPoint) -> BoundaryPointPosition {
    debug_assert_eq!(
      self.tree_root_for_range(a.node),
      self.tree_root_for_range(b.node),
      "boundary point position requires nodes in the same tree root"
    );

    if a.node == b.node {
      return match a.offset.cmp(&b.offset) {
        Ordering::Less => BoundaryPointPosition::Before,
        Ordering::Equal => BoundaryPointPosition::Equal,
        Ordering::Greater => BoundaryPointPosition::After,
      };
    }

    if self.compare_tree_order_for_range(a.node, b.node) == Ordering::Greater {
      // If nodeA is following nodeB, invert the comparison in the other direction.
      return invert_boundary_point_position(self.boundary_point_position(b, a));
    }

    if self.is_ancestor_for_range(a.node, b.node) {
      let mut child = b.node;
      let mut remaining = self.nodes.len() + 1;
      while remaining > 0 {
        remaining -= 1;
        if self.range_parent(child) == Some(a.node) {
          break;
        }
        let Some(parent) = self.range_parent(child) else {
          break;
        };
        child = parent;
      }

      if let Some(index) = self.node_index(child) {
        if index < a.offset {
          return BoundaryPointPosition::After;
        }
      }
    }

    BoundaryPointPosition::Before
  }

  fn range_set_start_or_end(
    &mut self,
    range: RangeId,
    node: NodeId,
    offset: usize,
    is_start: bool,
  ) -> DomResult<()> {
    let node_kind = &self.node_checked(node)?.kind;
    if matches!(node_kind, &NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeType);
    }

    let node_len = self.node_length(node)?;
    if offset > node_len {
      return Err(DomError::IndexSizeError);
    }

    let boundary_point = BoundaryPoint { node, offset };
    let range_root = {
      let range = self.range(range)?;
      self.tree_root_for_range(range.start.node)
    };
    let node_root = self.tree_root_for_range(node);

    if is_start {
      let end = self.range(range)?.end;
      let should_set_end = range_root != node_root
        || self.boundary_point_position(boundary_point, end) == BoundaryPointPosition::After;
      let range = self.range_mut(range)?;
      if should_set_end {
        range.end = boundary_point;
      }
      range.start = boundary_point;
    } else {
      let start = self.range(range)?.start;
      let should_set_start = range_root != node_root
        || self.boundary_point_position(boundary_point, start) == BoundaryPointPosition::Before;
      let range = self.range_mut(range)?;
      if should_set_start {
        range.start = boundary_point;
      }
      range.end = boundary_point;
    }

    Ok(())
  }

  fn is_inclusive_descendant_for_range(&self, node: NodeId, ancestor: NodeId) -> bool {
    if self.tree_root_for_range(node) != self.tree_root_for_range(ancestor) {
      // `dom2` stores ShadowRoot nodes in the main tree with a parent pointer to the host, but Range
      // algorithms use the DOM notion of root (ShadowRoot is a separate tree). Without this root
      // check a node inside a shadow tree would incorrectly be treated as a descendant of the host.
      return false;
    }

    let mut current = Some(node);
    let mut remaining = self.nodes.len() + 1;
    while let Some(id) = current {
      if remaining == 0 {
        return false;
      }
      remaining -= 1;

      if id == ancestor {
        return true;
      }
      current = self.range_parent(id);
    }
    false
  }

  /// Live range pre-remove steps.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-live-range-pre-remove
  pub(super) fn live_range_pre_remove_steps(&mut self, node: NodeId, parent: NodeId, index: usize) {
    let boundary_point = BoundaryPoint {
      node: parent,
      offset: index,
    };
    if self.ranges.is_empty() {
      return;
    }

    // The algorithm performs multiple passes over the set of live ranges. Since `FxHashMap` does
    // not support stable indexed access, snapshot the keys up-front.
    let range_ids: Vec<RangeId> = self.ranges.keys().copied().collect();

    for id in &range_ids {
      let start_node = match self.ranges.get(id) {
        Some(range) => range.start.node,
        None => continue,
      };
      if self.is_inclusive_descendant_for_range(start_node, node) {
        if let Some(range) = self.ranges.get_mut(id) {
          range.start = boundary_point;
        }
      }
    }

    for id in &range_ids {
      let end_node = match self.ranges.get(id) {
        Some(range) => range.end.node,
        None => continue,
      };
      if self.is_inclusive_descendant_for_range(end_node, node) {
        if let Some(range) = self.ranges.get_mut(id) {
          range.end = boundary_point;
        }
      }
    }

    for id in &range_ids {
      let start = match self.ranges.get(id) {
        Some(range) => range.start,
        None => continue,
      };
      if start.node == parent && start.offset > index {
        if let Some(range) = self.ranges.get_mut(id) {
          range.start.offset = start.offset.saturating_sub(1);
        }
      }
    }

    for id in &range_ids {
      let end = match self.ranges.get(id) {
        Some(range) => range.end,
        None => continue,
      };
      if end.node == parent && end.offset > index {
        if let Some(range) = self.ranges.get_mut(id) {
          range.end.offset = end.offset.saturating_sub(1);
        }
      }
    }
  }

  /// Live range "replace data" steps.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-cd-replace
  pub(super) fn live_range_replace_data_steps(
    &mut self,
    node: NodeId,
    offset: usize,
    removed_len: usize,
    inserted_len: usize,
  ) {
    if self.ranges.is_empty() {
      return;
    }
    if removed_len == 0 && inserted_len == 0 {
      return;
    }

    let end = offset.saturating_add(removed_len);

    // 8. For each live range whose start node is node and start offset is greater than offset but
    // less than or equal to offset + count: set its start offset to offset.
    for range in self.ranges.iter_mut() {
      if range.start.node == node && range.start.offset > offset && range.start.offset <= end {
        range.start.offset = offset;
      }
    }

    // 9. For each live range whose end node is node and end offset is greater than offset but less
    // than or equal to offset + count: set its end offset to offset.
    for range in self.ranges.iter_mut() {
      if range.end.node == node && range.end.offset > offset && range.end.offset <= end {
        range.end.offset = offset;
      }
    }

    // 10/11. For each live range whose start/end offset is greater than offset + count: increase
    // by data's length and decrease by count.
    if inserted_len >= removed_len {
      let delta = inserted_len - removed_len;
      if delta != 0 {
        for range in self.ranges.iter_mut() {
          if range.start.node == node && range.start.offset > end {
            range.start.offset = range.start.offset.saturating_add(delta);
          }
        }
        for range in self.ranges.iter_mut() {
          if range.end.node == node && range.end.offset > end {
            range.end.offset = range.end.offset.saturating_add(delta);
          }
        }
      }
    } else {
      let delta = removed_len - inserted_len;
      for range in self.ranges.iter_mut() {
        if range.start.node == node && range.start.offset > end {
          range.start.offset = range.start.offset.saturating_sub(delta);
        }
      }
      for range in self.ranges.iter_mut() {
        if range.end.node == node && range.end.offset > end {
          range.end.offset = range.end.offset.saturating_sub(delta);
        }
      }
    }
  }
}
