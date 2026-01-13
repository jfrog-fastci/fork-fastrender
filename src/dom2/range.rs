use super::{Document, DomError, DomResult, LiveRangeId, NodeId, NodeKind};
use std::cmp::Ordering;
use std::collections::HashMap;

/// Compare two dom2 nodes in *current DOM tree order* with WHATWG DOM Range ShadowRoot semantics.
///
/// This is a lightweight wrapper around the tree-order logic used by Range algorithms:
/// - Only nodes in the same Range tree root (Document or ShadowRoot) are comparable.
/// - Detached nodes are treated as unordered (`Ordering::Equal`) so callers can prune them.
/// - If the tree structure is inconsistent (e.g. missing parent/child linkage), nodes are treated
///   as unordered rather than falling back to `NodeId::index()` ordering.
///
/// Callers **must** handle the `Ordering::Equal` fallback case for distinct nodes (e.g. by dropping
/// selection points/ranges that are disconnected or cross a shadow boundary).
pub(crate) fn cmp_dom2_nodes(dom: &Document, a: NodeId, b: NodeId) -> Ordering {
  if a == b {
    return Ordering::Equal;
  }

  // Treat detached/out-of-bounds nodes as unordered.
  if !dom.is_connected(a) || !dom.is_connected(b) {
    return Ordering::Equal;
  }

  // Range algorithms treat ShadowRoot as the root of a separate tree, so do not attempt to order
  // nodes across shadow boundaries.
  if dom.tree_root_for_range(a) != dom.tree_root_for_range(b) {
    return Ordering::Equal;
  }

  // We want true DOM tree order, not arena insertion order. `compare_tree_order_for_range` has an
  // internal `NodeId::index()` fallback when the tree is inconsistent; that is useful as a
  // deterministic tie-breaker for internal Range algorithms, but for selection ordering we treat
  // such nodes as unordered and let callers prune them.
  fn path_to_root(doc: &Document, node: NodeId) -> Option<Vec<NodeId>> {
    let mut out: Vec<NodeId> = Vec::new();
    let mut current = Some(node);
    let mut remaining = doc.nodes.len() + 1;
    while let Some(id) = current {
      if remaining == 0 {
        return None;
      }
      remaining -= 1;
      out.push(id);
      current = doc.range_parent(id);
    }
    out.reverse();
    Some(out)
  }

  let Some(path_a) = path_to_root(dom, a) else {
    return Ordering::Equal;
  };
  let Some(path_b) = path_to_root(dom, b) else {
    return Ordering::Equal;
  };

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

  let child_a = path_a[i];
  let child_b = path_b[i];
  match (dom.node_index(child_a), dom.node_index(child_b)) {
    (Some(a_idx), Some(b_idx)) => a_idx.cmp(&b_idx),
    _ => Ordering::Equal,
  }
}

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
  pub(super) start: BoundaryPoint,
  pub(super) end: BoundaryPoint,
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
  #[inline]
  fn parent_is_element_like_for_range(&self, parent: NodeId) -> bool {
    matches!(
      self.nodes.get(parent.index()).map(|n| &n.kind),
      Some(NodeKind::Element { .. } | NodeKind::Slot { .. })
    )
  }

  #[inline]
  fn is_shadow_root_node(&self, node: NodeId) -> bool {
    matches!(
      self.nodes.get(node.index()).map(|n| &n.kind),
      Some(NodeKind::ShadowRoot { .. })
    )
  }

  #[inline]
  fn is_tree_child_for_range(&self, parent: NodeId, child: NodeId) -> bool {
    if self.parent_is_element_like_for_range(parent) && self.is_shadow_root_node(child) {
      return false;
    }
    true
  }

  fn tree_child_count_for_range(&self, parent: NodeId) -> usize {
    let mut count = 0usize;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);
    let children = &self.nodes[parent.index()].children;
    for &child in children {
      if self.nodes[child.index()].parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }
      count += 1;
    }
    count
  }

  /// Return the `index`th tree child of `parent` for Range algorithms.
  ///
  /// Range boundary-point offsets into element-like nodes are defined in terms of light-DOM tree
  /// children. `dom2` stores an attached `ShadowRoot` as a host child for traversal, but it must
  /// not be visible to Range algorithms as a tree child.
  pub(super) fn tree_child_for_range(&self, parent: NodeId, index: usize) -> Option<NodeId> {
    let parent_node = self.nodes.get(parent.index())?;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);

    let mut current = 0usize;
    for &child in parent_node.children.iter() {
      let Some(child_node) = self.nodes.get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }

      if current == index {
        return Some(child);
      }
      current += 1;
    }

    None
  }

  pub(super) fn tree_child_index_for_range(&self, parent: NodeId, child: NodeId) -> Option<usize> {
    if self.nodes.get(child.index())?.parent != Some(parent) {
      return None;
    }
    if !self.is_tree_child_for_range(parent, child) {
      return None;
    }

    let mut idx = 0usize;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);
    for &c in &self.nodes.get(parent.index())?.children {
      if self.nodes.get(c.index())?.parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(c) {
        continue;
      }
      if c == child {
        return Some(idx);
      }
      idx += 1;
    }
    None
  }

  fn tree_children_for_range(&self, parent: NodeId) -> Vec<NodeId> {
    let Some(parent_node) = self.nodes.get(parent.index()) else {
      return Vec::new();
    };

    parent_node
      .children
      .iter()
      .copied()
      .filter(|&child| {
        let Some(child_node) = self.nodes.get(child.index()) else {
          return false;
        };
        child_node.parent == Some(parent) && self.is_tree_child_for_range(parent, child)
      })
      .collect()
  }

  /// Map an index into `parent.children` (including ShadowRoot nodes) to a DOM Range "tree child"
  /// index.
  ///
  /// For element-like parents, this excludes ShadowRoot children (ShadowRoots are not part of the
  /// light DOM `childNodes` list, and must not contribute to Range boundary point offsets).
  pub(super) fn tree_child_index_from_raw_index_for_range(
    &self,
    parent: NodeId,
    raw_index: usize,
  ) -> usize {
    let mut idx = 0usize;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);
    let children = &self.nodes[parent.index()].children;
    let end = raw_index.min(children.len());
    for &child in children.iter().take(end) {
      if self.nodes[child.index()].parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }
      idx += 1;
    }
    idx
  }

  pub(super) fn inserted_tree_children_count_for_range(
    &self,
    parent: NodeId,
    inserted: &[NodeId],
  ) -> usize {
    if !self.parent_is_element_like_for_range(parent) {
      return inserted.len();
    }
    inserted
      .iter()
      .filter(|&&child| !self.is_shadow_root_node(child))
      .count()
  }

  /// WHATWG DOM `Node.compareDocumentPosition()` for nodes within this `dom2::Document`.
  ///
  /// This is a pure helper for bindings. Callers are responsible for handling comparisons across
  /// distinct `dom2::Document` allocations (different JS `Document` objects that are backed by
  /// different arenas).
  ///
  /// Shadow DOM note: `dom2` stores `ShadowRoot` nodes as children of their host element, but per
  /// the DOM Standard a shadow root is the root of a separate tree (its parent is null). This
  /// implementation therefore uses the Range-specific "tree root" helpers (`tree_root_for_range` /
  /// `range_parent`) so comparisons do not cross shadow boundaries.
  pub fn compare_document_position(&self, a: NodeId, b: NodeId) -> u16 {
    const DOCUMENT_POSITION_DISCONNECTED: u16 = 0x01;
    const DOCUMENT_POSITION_PRECEDING: u16 = 0x02;
    const DOCUMENT_POSITION_FOLLOWING: u16 = 0x04;
    const DOCUMENT_POSITION_CONTAINS: u16 = 0x08;
    const DOCUMENT_POSITION_CONTAINED_BY: u16 = 0x10;
    const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: u16 = 0x20;

    if a == b {
      return 0;
    }

    let root_a = self.tree_root_for_range(a);
    let root_b = self.tree_root_for_range(b);
    if root_a != root_b {
      // Per DOM, disconnected nodes must include DISCONNECTED and IMPLEMENTATION_SPECIFIC, plus an
      // arbitrary but consistent PRECEDING/FOLLOWING bit. We use the tree root node id as a stable
      // tie-breaker.
      let mut out = DOCUMENT_POSITION_DISCONNECTED | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
      out |= if root_a.index() < root_b.index() {
        DOCUMENT_POSITION_FOLLOWING
      } else {
        DOCUMENT_POSITION_PRECEDING
      };
      return out;
    }

    // DOM semantics:
    // - If `b` is an ancestor of `a`, then "other contains this" => CONTAINS | PRECEDING.
    // - If `b` is a descendant of `a`, then "other is contained by this" => CONTAINED_BY | FOLLOWING.
    if self.is_ancestor_for_range(b, a) {
      return DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_PRECEDING;
    }
    if self.is_ancestor_for_range(a, b) {
      return DOCUMENT_POSITION_CONTAINED_BY | DOCUMENT_POSITION_FOLLOWING;
    }

    match self.compare_tree_order_for_range(a, b) {
      Ordering::Less => DOCUMENT_POSITION_FOLLOWING,
      Ordering::Greater => DOCUMENT_POSITION_PRECEDING,
      Ordering::Equal => 0,
    }
  }

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

  #[cfg(test)]
  pub(crate) fn range_state_len_for_test(&self) -> usize {
    self.ranges.len()
  }

  pub(crate) fn create_range_for_id(&mut self, id: RangeId) {
    self.insert_range_state(id);
  }

  pub(crate) fn remove_range(&mut self, id: RangeId) {
    self.ranges.remove(&id);
  }

  /// Remap `NodeId` references stored in all live ranges.
  ///
  /// This is intended for clone+mapping operations that preserve JS wrapper identity by updating
  /// wrapper objects to point at new `NodeId` values (e.g. cross-document adoption approximations).
  ///
  /// Any range endpoints whose container node is present in `mapping` are updated in-place. Offsets
  /// are preserved.
  pub(crate) fn range_remap_node_ids(&mut self, mapping: &HashMap<NodeId, NodeId>) {
    if mapping.is_empty() || self.ranges.is_empty() {
      return;
    }
    for range in self.ranges.values_mut() {
      if let Some(&new_start) = mapping.get(&range.start.node) {
        range.start.node = new_start;
      }
      if let Some(&new_end) = mapping.get(&range.end.node) {
        range.end.node = new_end;
      }
    }
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

  /// Whether the range is collapsed (its start and end boundary points are equal).
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-collapsed
  pub fn range_collapsed(&self, range: RangeId) -> DomResult<bool> {
    let range = self.range(range)?;
    Ok(range.start == range.end)
  }

  /// Returns the common ancestor container of a live range.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-commonancestorcontainer
  pub fn range_common_ancestor_container(&self, range: RangeId) -> DomResult<NodeId> {
    let start = self.range_start_container(range)?;
    let end = self.range_end_container(range)?;

    // DOM: "Let container be start node. While container is not an inclusive ancestor of end node,
    // set container to its parent."
    let mut container = start;
    let mut remaining = self.nodes.len().saturating_add(1);
    while remaining > 0 {
      remaining -= 1;
      if self.is_inclusive_descendant_for_range(end, container) {
        return Ok(container);
      }
      match self.range_parent(container) {
        Some(parent) => container = parent,
        None => return Ok(container),
      }
    }

    // Corruption/cycle guard: fall back to the best known node.
    Ok(container)
  }

  /// Compare the boundary points of two ranges per `Range.compareBoundaryPoints()`.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-compareboundarypoints
  pub fn range_compare_boundary_points(
    &self,
    range_a: RangeId,
    how: u16,
    range_b: RangeId,
  ) -> DomResult<i16> {
    let range_a = self.range(range_a)?;
    let range_b = self.range(range_b)?;

    // `how` is an `unsigned short` in WebIDL; callers are expected to convert before invoking this
    // helper. We still validate the allowed values (0..=3) per the DOM Standard.
    if how > 3 {
      return Err(DomError::NotSupportedError);
    }

    // DOM `Range` objects always have both endpoints in the same root (ShadowRoot-aware); use the
    // start container roots as the range roots.
    if self.tree_root_for_range(range_a.start.node) != self.tree_root_for_range(range_b.start.node)
    {
      return Err(DomError::WrongDocumentError);
    }

    let (this_point, other_point) = match how {
      0 => (range_a.start, range_b.start), // START_TO_START
      1 => (range_a.end, range_b.start),   // START_TO_END
      2 => (range_a.end, range_b.end),     // END_TO_END
      3 => (range_a.start, range_b.end),   // END_TO_START
      _ => unreachable!("checked above"), // fastrender-allow-panic
    };

    Ok(match self.boundary_point_position(this_point, other_point) {
      BoundaryPointPosition::Before => -1,
      BoundaryPointPosition::Equal => 0,
      BoundaryPointPosition::After => 1,
    })
  }

  /// Return the stringification of `range` per the WHATWG DOM Range stringifier.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-stringifier
  pub fn range_to_string(&self, range: RangeId) -> DomResult<String> {
    let range = self.range(range)?;
    let start = range.start;
    let end = range.end;

    if start == end {
      return Ok(String::new());
    }

    let range_root = self.tree_root_for_range(start.node);
    if self.tree_root_for_range(end.node) != range_root {
      // Range endpoints are required to stay within the same tree root. If invariants are broken
      // (e.g. due to a bug elsewhere), fail closed rather than traversing across roots.
      return Ok(String::new());
    }

    // If both endpoints are in the same CharacterData node (Text/Comment/PI), return the substring
    // between offsets.
    if start.node == end.node && self.node_is_character_data_for_range(start.node) {
      return self.substring_character_data_for_range(
        start.node,
        start.offset,
        end.offset.saturating_sub(start.offset),
      );
    }

    let mut out = String::new();

    // If start node is a Text node, append the substring from the start offset to the end.
    {
      let node = self.node_checked(start.node)?;
      if matches!(&node.kind, NodeKind::Text { .. }) {
        let len = self.node_length(start.node)?;
        if start.offset < len {
          out.push_str(&self.substring_character_data_for_range(
            start.node,
            start.offset,
            len.saturating_sub(start.offset),
          )?);
        }
      }
    }

    // For each Text node contained in the range, in tree order, append its data.
    //
    // We traverse the range's tree root and filter by containment checks rather than attempting to
    // step from the start boundary to the end boundary. This is sufficient for WPT coverage and
    // avoids subtle shadow-root traversal pitfalls (`dom2` stores ShadowRoot nodes as children of
    // their host elements for renderer traversal).
    let mut stack: Vec<NodeId> = vec![range_root];
    while let Some(node_id) = stack.pop() {
      let node = self.node_checked(node_id)?;
      if let NodeKind::Text { content } = &node.kind {
        if self.is_node_contained_in_range(node_id, start, end)? {
          out.push_str(content);
        }
      }

      // Pre-order traversal: push children in reverse order.
      for &child in node.children.iter().rev() {
        // Shadow-root aware: never traverse across different tree roots.
        if self.tree_root_for_range(child) == range_root {
          stack.push(child);
        }
      }
    }

    // If end node is a Text node, append the substring from the start of the node to the end
    // offset.
    {
      let node = self.node_checked(end.node)?;
      if matches!(&node.kind, NodeKind::Text { .. }) && end.offset > 0 {
        out.push_str(&self.substring_character_data_for_range(end.node, 0, end.offset)?);
      }
    }

    Ok(out)
  }
  pub fn range_set_start(&mut self, range: RangeId, node: NodeId, offset: usize) -> DomResult<()> {
    self.range_set_start_or_end(range, node, offset, /* is_start */ true)
  }

  pub fn range_set_end(&mut self, range: RangeId, node: NodeId, offset: usize) -> DomResult<()> {
    self.range_set_start_or_end(range, node, offset, /* is_start */ false)
  }

  pub fn range_set_start_before(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    if matches!(&self.node_checked(node)?.kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }
    let parent = self
      .range_parent(node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    let index = self
      .tree_child_index_for_range(parent, node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    self.range_set_start(range, parent, index)
  }

  pub fn range_set_start_after(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    if matches!(&self.node_checked(node)?.kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }
    let parent = self
      .range_parent(node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    let index = self
      .tree_child_index_for_range(parent, node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    self.range_set_start(range, parent, index.saturating_add(1))
  }

  pub fn range_set_end_before(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    if matches!(&self.node_checked(node)?.kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }
    let parent = self
      .range_parent(node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    let index = self
      .tree_child_index_for_range(parent, node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    self.range_set_end(range, parent, index)
  }

  pub fn range_set_end_after(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    if matches!(&self.node_checked(node)?.kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }
    let parent = self
      .range_parent(node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    let index = self
      .tree_child_index_for_range(parent, node)
      .ok_or(DomError::InvalidNodeTypeError)?;
    self.range_set_end(range, parent, index.saturating_add(1))
  }

  /// Collapse a live range to one of its boundary points.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-collapse
  pub fn range_collapse(&mut self, range: RangeId, to_start: bool) -> DomResult<()> {
    let range_obj = self.range_mut(range)?;
    if to_start {
      range_obj.end = range_obj.start;
    } else {
      range_obj.start = range_obj.end;
    }
    Ok(())
  }

  /// Legacy `Range.prototype.detach()` hook.
  ///
  /// Per spec this is a no-op, but it must exist for compatibility.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-detach
  pub fn range_detach(&self, range: RangeId) -> DomResult<()> {
    let _ = self.range(range)?;
    Ok(())
  }

  pub fn range_select_node_contents(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    if matches!(&self.node_checked(node)?.kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }
    let len = self.node_length(node)?;
    self.range_set_start(range, node, 0)?;
    self.range_set_end(range, node, len)
  }

  /// Set the start/end of the live range to exactly contain `node`.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-selectnode
  pub fn range_select_node(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    self.range_set_start_before(range, node)?;
    self.range_set_end_after(range, node)
  }

  /// Clone a live range, returning a new independent range object with the same boundary points.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-clonerange
  pub fn range_clone_range(&mut self, range: RangeId) -> DomResult<RangeId> {
    let existing = self.range(range)?.clone();
    let id = self.live_mutation.alloc_live_range_id();
    let prev = self.ranges.insert(id, existing);
    debug_assert!(
      prev.is_none(),
      "range id collision: attempted to insert duplicate Range state"
    );
    Ok(id)
  }

  /// `Range.prototype.isPointInRange(node, offset)`.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-ispointinrange
  pub fn range_is_point_in_range(
    &self,
    range: RangeId,
    node: NodeId,
    offset: usize,
  ) -> DomResult<bool> {
    // Validate `node` exists (defensive; callers should only pass valid NodeIds).
    let _ = self.node_checked(node)?;

    let range = self.range(range)?;
    let range_root = self.tree_root_for_range(range.start.node);
    let node_root = self.tree_root_for_range(node);
    if range_root != node_root {
      // Spec: return false when the point is in a different tree root.
      return Ok(false);
    }

    if matches!(&self.node(node).kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }

    if offset > self.node_length(node)? {
      return Err(DomError::IndexSizeError);
    }

    let point = BoundaryPoint { node, offset };
    let start = range.start;
    let end = range.end;
    if matches!(
      self.boundary_point_position(point, start),
      BoundaryPointPosition::Before
    ) || matches!(
      self.boundary_point_position(point, end),
      BoundaryPointPosition::After
    ) {
      return Ok(false);
    }

    Ok(true)
  }

  /// `Range.prototype.comparePoint(node, offset)`.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-comparepoint
  pub fn range_compare_point(&self, range: RangeId, node: NodeId, offset: usize) -> DomResult<i16> {
    // Validate `node` exists (defensive; callers should only pass valid NodeIds).
    let _ = self.node_checked(node)?;

    let range = self.range(range)?;
    let range_root = self.tree_root_for_range(range.start.node);
    let node_root = self.tree_root_for_range(node);
    if range_root != node_root {
      return Err(DomError::WrongDocumentError);
    }

    if matches!(&self.node(node).kind, NodeKind::Doctype { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }

    if offset > self.node_length(node)? {
      return Err(DomError::IndexSizeError);
    }

    let point = BoundaryPoint { node, offset };
    let start = range.start;
    let end = range.end;

    if matches!(
      self.boundary_point_position(point, start),
      BoundaryPointPosition::Before
    ) {
      return Ok(-1);
    }
    if matches!(
      self.boundary_point_position(point, end),
      BoundaryPointPosition::After
    ) {
      return Ok(1);
    }
    Ok(0)
  }

  /// `Range.prototype.intersectsNode(node)`.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-intersectsnode
  pub fn range_intersects_node(&self, range: RangeId, node: NodeId) -> DomResult<bool> {
    // Validate `node` exists (defensive; callers should only pass valid NodeIds).
    let _ = self.node_checked(node)?;

    let range = self.range(range)?;
    let range_root = self.tree_root_for_range(range.start.node);
    let node_root = self.tree_root_for_range(node);
    if range_root != node_root {
      return Ok(false);
    }

    let Some(parent) = self.range_parent(node) else {
      // Spec: if the node has no parent, it intersects the range (assuming same root).
      return Ok(true);
    };

    let Some(offset) = self.node_index(node) else {
      return Ok(false);
    };

    let start = range.start;
    let end = range.end;

    let before_end = matches!(
      self.boundary_point_position(
        BoundaryPoint { node: parent, offset },
        end
      ),
      BoundaryPointPosition::Before
    );

    let after_start = matches!(
      self.boundary_point_position(
        BoundaryPoint {
          node: parent,
          offset: offset.saturating_add(1),
        },
        start
      ),
      BoundaryPointPosition::After
    );

    Ok(before_end && after_start)
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

  pub(crate) fn node_length(&self, node: NodeId) -> DomResult<usize> {
    let node_id = node;
    let node = self.node_checked(node_id)?;
    Ok(match &node.kind {
      NodeKind::Document { .. }
      | NodeKind::DocumentFragment
      | NodeKind::ShadowRoot { .. } => self.tree_child_count_for_range(node_id),
      NodeKind::Slot { .. } | NodeKind::Element { .. } => self.tree_child_count_for_range(node_id),
      NodeKind::Text { content } | NodeKind::Comment { content } => content.encode_utf16().count(),
      NodeKind::ProcessingInstruction { data, .. } => data.encode_utf16().count(),
      NodeKind::Doctype { .. } => 0,
    })
  }

  /// Compare two boundary points within the same tree root (DOM Range ordering).
  ///
  /// Returns `Ordering::Less` when `a` is before `b`, `Ordering::Equal` when equal, and
  /// `Ordering::Greater` when after.
  ///
  /// Callers are responsible for checking the two boundary points share the same
  /// [`tree_root_for_range`]; this mirrors the DOM Standard's requirements and keeps the internal
  /// comparison helper infallible.
  pub(crate) fn compare_boundary_points(&self, a: BoundaryPoint, b: BoundaryPoint) -> Ordering {
    match self.boundary_point_position(a, b) {
      BoundaryPointPosition::Before => Ordering::Less,
      BoundaryPointPosition::Equal => Ordering::Equal,
      BoundaryPointPosition::After => Ordering::Greater,
    }
  }

  fn node_index(&self, node: NodeId) -> Option<usize> {
    let parent = self.range_parent(node)?;
    self.tree_child_index_for_range(parent, node)
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

    let child_a = path_a[i];
    let child_b = path_b[i];

    let idx_a = self.node_index(child_a);
    let idx_b = self.node_index(child_b);

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
      return Err(DomError::InvalidNodeTypeError);
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

  #[inline]
  fn node_is_character_data_for_range(&self, node: NodeId) -> bool {
    self
      .nodes
      .get(node.index())
      .is_some_and(|n| matches!(n.kind, NodeKind::Text { .. } | NodeKind::Comment { .. } | NodeKind::ProcessingInstruction { .. }))
  }

  fn character_data_string_for_range(&self, node: NodeId) -> DomResult<&str> {
    let node = self.node_checked(node)?;
    match &node.kind {
      NodeKind::Text { content } | NodeKind::Comment { content } => Ok(content.as_str()),
      NodeKind::ProcessingInstruction { data, .. } => Ok(data.as_str()),
      _ => Err(DomError::InvalidNodeTypeError),
    }
  }

  fn set_character_data_string_for_range(&mut self, node: NodeId, data: String) -> DomResult<()> {
    let node = self.node_checked_mut(node)?;
    match &mut node.kind {
      NodeKind::Text { content } | NodeKind::Comment { content } => {
        *content = data;
        Ok(())
      }
      NodeKind::ProcessingInstruction { data: value, .. } => {
        *value = data;
        Ok(())
      }
      _ => Err(DomError::InvalidNodeTypeError),
    }
  }

  fn substring_character_data_for_range(
    &self,
    node: NodeId,
    offset: usize,
    count: usize,
  ) -> DomResult<String> {
    let data = self.character_data_string_for_range(node)?;
    let units: Vec<u16> = data.encode_utf16().collect();
    if offset > units.len() {
      return Err(DomError::IndexSizeError);
    }
    let end = offset.saturating_add(count).min(units.len());
    Ok(String::from_utf16_lossy(&units[offset..end]))
  }

  fn append_child_quiet_for_range(&mut self, parent: NodeId, child: NodeId) -> DomResult<()> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    // Internal algorithms only use this helper for freshly created / detached clones.
    debug_assert!(
      self.nodes[child.index()].parent.is_none(),
      "append_child_quiet_for_range requires a detached child"
    );
    self.nodes[child.index()].parent = Some(parent);
    self.nodes[parent.index()].children.push(child);
    Ok(())
  }

  fn append_document_fragment_contents_quiet_for_range(
    &mut self,
    parent: NodeId,
    fragment: NodeId,
  ) -> DomResult<()> {
    self.node_checked(parent)?;
    self.node_checked(fragment)?;
    let moved_children = std::mem::take(&mut self.nodes[fragment.index()].children);
    if moved_children.is_empty() {
      return Ok(());
    }
    for &child in &moved_children {
      if let Some(node) = self.nodes.get_mut(child.index()) {
        node.parent = Some(parent);
      }
    }
    self.nodes[parent.index()].children.extend(moved_children);
    Ok(())
  }

  fn is_node_partially_contained_in_range(
    &self,
    node: NodeId,
    start_node: NodeId,
    end_node: NodeId,
  ) -> bool {
    let start_in = self.is_inclusive_descendant_for_range(start_node, node);
    let end_in = self.is_inclusive_descendant_for_range(end_node, node);
    start_in ^ end_in
  }

  fn is_node_contained_in_range(
    &self,
    node: NodeId,
    start: BoundaryPoint,
    end: BoundaryPoint,
  ) -> DomResult<bool> {
    let range_root = self.tree_root_for_range(start.node);
    if self.tree_root_for_range(node) != range_root {
      return Ok(false);
    }
    let len = self.node_length(node)?;
    let after_start =
      self.boundary_point_position(BoundaryPoint { node, offset: 0 }, start) == BoundaryPointPosition::After;
    let before_end = self.boundary_point_position(BoundaryPoint { node, offset: len }, end)
      == BoundaryPointPosition::Before;
    Ok(after_start && before_end)
  }

  fn clone_contents_between(&mut self, start: BoundaryPoint, end: BoundaryPoint) -> DomResult<NodeId> {
    // Spec: https://dom.spec.whatwg.org/#concept-range-clone
    let fragment = self.create_document_fragment();
    if start == end {
      return Ok(fragment);
    }

    let (original_start_node, original_start_offset) = (start.node, start.offset);
    let (original_end_node, original_end_offset) = (end.node, end.offset);

    if original_start_node == original_end_node && self.node_is_character_data_for_range(original_start_node)
    {
      let clone = self.clone_node(original_start_node, /* deep */ false)?;
      let data = self.substring_character_data_for_range(
        original_start_node,
        original_start_offset,
        original_end_offset.saturating_sub(original_start_offset),
      )?;
      self.set_character_data_string_for_range(clone, data)?;
      self.append_child_quiet_for_range(fragment, clone)?;
      return Ok(fragment);
    }

    // 5-6. Compute common ancestor.
    let mut common_ancestor = original_start_node;
    let mut remaining = self.nodes.len() + 1;
    while remaining > 0 && !self.is_inclusive_descendant_for_range(original_end_node, common_ancestor) {
      remaining -= 1;
      let Some(parent) = self.range_parent(common_ancestor) else {
        break;
      };
      common_ancestor = parent;
    }

    let start_node = original_start_node;
    let end_node = original_end_node;

    let original_start_is_inclusive_ancestor_of_end =
      self.is_inclusive_descendant_for_range(original_end_node, original_start_node);
    let original_end_is_inclusive_ancestor_of_start =
      self.is_inclusive_descendant_for_range(original_start_node, original_end_node);

    let common_children: Vec<NodeId> = self.tree_children_for_range(common_ancestor);

    let first_partially_contained_child = if original_start_is_inclusive_ancestor_of_end {
      None
    } else {
      common_children
        .iter()
        .copied()
        .find(|&child| self.is_node_partially_contained_in_range(child, start_node, end_node))
    };

    let last_partially_contained_child = if original_end_is_inclusive_ancestor_of_start {
      None
    } else {
      common_children
        .iter()
        .rev()
        .copied()
        .find(|&child| self.is_node_partially_contained_in_range(child, start_node, end_node))
    };

    let mut contained_children: Vec<NodeId> = Vec::new();
    for child in common_children.iter().copied() {
      if self.is_node_contained_in_range(child, start, end)? {
        contained_children.push(child);
      }
    }

    if contained_children.iter().any(|&id| matches!(self.nodes[id.index()].kind, NodeKind::Doctype { .. })) {
      return Err(DomError::HierarchyRequestError);
    }

    if let Some(first) = first_partially_contained_child {
      if self.node_is_character_data_for_range(first) {
        let clone = self.clone_node(original_start_node, /* deep */ false)?;
        let start_len = self.node_length(original_start_node)?;
        let data = self.substring_character_data_for_range(
          original_start_node,
          original_start_offset,
          start_len.saturating_sub(original_start_offset),
        )?;
        self.set_character_data_string_for_range(clone, data)?;
        self.append_child_quiet_for_range(fragment, clone)?;
      } else {
        let clone = self.clone_node(first, /* deep */ false)?;
        self.append_child_quiet_for_range(fragment, clone)?;

        let end_offset = self.node_length(first)?;
        let subfragment = self.clone_contents_between(
          BoundaryPoint {
            node: original_start_node,
            offset: original_start_offset,
          },
          BoundaryPoint {
            node: first,
            offset: end_offset,
          },
        )?;
        self.append_document_fragment_contents_quiet_for_range(clone, subfragment)?;
      }
    }

    for child in contained_children {
      let clone = self.clone_node(child, /* deep */ true)?;
      self.append_child_quiet_for_range(fragment, clone)?;
    }

    if let Some(last) = last_partially_contained_child {
      if self.node_is_character_data_for_range(last) {
        let clone = self.clone_node(original_end_node, /* deep */ false)?;
        let data = self.substring_character_data_for_range(original_end_node, 0, original_end_offset)?;
        self.set_character_data_string_for_range(clone, data)?;
        self.append_child_quiet_for_range(fragment, clone)?;
      } else {
        let clone = self.clone_node(last, /* deep */ false)?;
        self.append_child_quiet_for_range(fragment, clone)?;

        let subfragment = self.clone_contents_between(
          BoundaryPoint { node: last, offset: 0 },
          BoundaryPoint {
            node: original_end_node,
            offset: original_end_offset,
          },
        )?;
        self.append_document_fragment_contents_quiet_for_range(clone, subfragment)?;
      }
    }

    Ok(fragment)
  }

  pub fn range_clone_contents(&mut self, range: RangeId) -> DomResult<NodeId> {
    let (start, end) = {
      let r = self.range(range)?;
      (r.start, r.end)
    };
    self.clone_contents_between(start, end)
  }

  fn extract_contents_between_impl(
    &mut self,
    start: BoundaryPoint,
    end: BoundaryPoint,
  ) -> DomResult<(NodeId, Option<BoundaryPoint>)> {
    // Spec: https://dom.spec.whatwg.org/#concept-range-extract
    let fragment = self.create_document_fragment();
    if start == end {
      return Ok((fragment, None));
    }

    let (original_start_node, original_start_offset) = (start.node, start.offset);
    let (original_end_node, original_end_offset) = (end.node, end.offset);

    if original_start_node == original_end_node && self.node_is_character_data_for_range(original_start_node)
    {
      let clone = self.clone_node(original_start_node, /* deep */ false)?;
      let data = self.substring_character_data_for_range(
        original_start_node,
        original_start_offset,
        original_end_offset.saturating_sub(original_start_offset),
      )?;
      self.set_character_data_string_for_range(clone, data)?;
      self.append_child_quiet_for_range(fragment, clone)?;
      let _ = self.replace_data(
        original_start_node,
        original_start_offset,
        original_end_offset.saturating_sub(original_start_offset),
        "",
      )?;
      return Ok((
        fragment,
        Some(BoundaryPoint {
          node: original_start_node,
          offset: original_start_offset,
        }),
      ));
    }

    // 5-6. Compute common ancestor.
    let mut common_ancestor = original_start_node;
    let mut remaining = self.nodes.len() + 1;
    while remaining > 0 && !self.is_inclusive_descendant_for_range(original_end_node, common_ancestor) {
      remaining -= 1;
      let Some(parent) = self.range_parent(common_ancestor) else {
        break;
      };
      common_ancestor = parent;
    }

    let start_node = original_start_node;
    let end_node = original_end_node;

    let original_start_is_inclusive_ancestor_of_end =
      self.is_inclusive_descendant_for_range(original_end_node, original_start_node);
    let original_end_is_inclusive_ancestor_of_start =
      self.is_inclusive_descendant_for_range(original_start_node, original_end_node);

    let common_children: Vec<NodeId> = self.tree_children_for_range(common_ancestor);

    let first_partially_contained_child = if original_start_is_inclusive_ancestor_of_end {
      None
    } else {
      common_children
        .iter()
        .copied()
        .find(|&child| self.is_node_partially_contained_in_range(child, start_node, end_node))
    };

    let last_partially_contained_child = if original_end_is_inclusive_ancestor_of_start {
      None
    } else {
      common_children
        .iter()
        .rev()
        .copied()
        .find(|&child| self.is_node_partially_contained_in_range(child, start_node, end_node))
    };

    let mut contained_children: Vec<NodeId> = Vec::new();
    for child in common_children.iter().copied() {
      if self.is_node_contained_in_range(child, start, end)? {
        contained_children.push(child);
      }
    }

    if contained_children.iter().any(|&id| matches!(self.nodes[id.index()].kind, NodeKind::Doctype { .. })) {
      return Err(DomError::HierarchyRequestError);
    }

    // 13-15. Compute where to collapse the range after extraction.
    let collapse_point = if original_start_is_inclusive_ancestor_of_end {
      BoundaryPoint {
        node: original_start_node,
        offset: original_start_offset,
      }
    } else {
      let mut reference_node = original_start_node;
      let mut remaining = self.nodes.len() + 1;
      while remaining > 0 {
        remaining -= 1;
        let Some(parent) = self.range_parent(reference_node) else {
          break;
        };
        if self.is_inclusive_descendant_for_range(original_end_node, parent) {
          break;
        }
        reference_node = parent;
      }

      if let Some(new_node) = self.range_parent(reference_node) {
        let idx = self.node_index(reference_node).unwrap_or(0);
        BoundaryPoint {
          node: new_node,
          offset: idx.saturating_add(1),
        }
      } else {
        BoundaryPoint {
          node: original_start_node,
          offset: original_start_offset,
        }
      }
    };

    // 16-20. Mutate the tree while building `fragment`.
    if let Some(first) = first_partially_contained_child {
      if self.node_is_character_data_for_range(first) {
        let clone = self.clone_node(original_start_node, /* deep */ false)?;
        let start_len = self.node_length(original_start_node)?;
        let data = self.substring_character_data_for_range(
          original_start_node,
          original_start_offset,
          start_len.saturating_sub(original_start_offset),
        )?;
        self.set_character_data_string_for_range(clone, data)?;
        self.append_child_quiet_for_range(fragment, clone)?;
        let _ = self.replace_data(
          original_start_node,
          original_start_offset,
          start_len.saturating_sub(original_start_offset),
          "",
        )?;
      } else {
        let clone = self.clone_node(first, /* deep */ false)?;
        self.append_child_quiet_for_range(fragment, clone)?;
        let end_offset = self.node_length(first)?;
        let subfragment = self.extract_contents_between(
          BoundaryPoint {
            node: original_start_node,
            offset: original_start_offset,
          },
          BoundaryPoint {
            node: first,
            offset: end_offset,
          },
        )?;
        self.append_document_fragment_contents_quiet_for_range(clone, subfragment)?;
      }
    }

    for child in contained_children {
      let _ = self.append_child(fragment, child)?;
    }

    if let Some(last) = last_partially_contained_child {
      if self.node_is_character_data_for_range(last) {
        let clone = self.clone_node(original_end_node, /* deep */ false)?;
        let data = self.substring_character_data_for_range(original_end_node, 0, original_end_offset)?;
        self.set_character_data_string_for_range(clone, data)?;
        self.append_child_quiet_for_range(fragment, clone)?;
        let _ = self.replace_data(original_end_node, 0, original_end_offset, "")?;
      } else {
        let clone = self.clone_node(last, /* deep */ false)?;
        self.append_child_quiet_for_range(fragment, clone)?;
        let subfragment = self.extract_contents_between(
          BoundaryPoint { node: last, offset: 0 },
          BoundaryPoint {
            node: original_end_node,
            offset: original_end_offset,
          },
        )?;
        self.append_document_fragment_contents_quiet_for_range(clone, subfragment)?;
      }
    }

    Ok((fragment, Some(collapse_point)))
  }

  fn extract_contents_between(&mut self, start: BoundaryPoint, end: BoundaryPoint) -> DomResult<NodeId> {
    let (fragment, _collapse) = self.extract_contents_between_impl(start, end)?;
    Ok(fragment)
  }

  pub fn range_extract_contents(&mut self, range: RangeId) -> DomResult<NodeId> {
    let (start, end) = {
      let r = self.range(range)?;
      (r.start, r.end)
    };
    let (fragment, collapse_to) = self.extract_contents_between_impl(start, end)?;
    if let Some(bp) = collapse_to {
      let r = self.range_mut(range)?;
      r.start = bp;
      r.end = bp;
    }
    Ok(fragment)
  }

  fn tree_child_at_for_range(&self, parent: NodeId, index: usize) -> Option<NodeId> {
    let parent_node = self.nodes.get(parent.index())?;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);
    let mut idx = 0usize;
    for &child in parent_node.children.iter() {
      let child_node = self.nodes.get(child.index())?;
      if child_node.parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }
      if idx == index {
        return Some(child);
      }
      idx += 1;
    }
    None
  }

  fn range_next_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.range_parent(node)?;
    let parent_node = self.nodes.get(parent.index())?;
    let parent_is_element_like = self.parent_is_element_like_for_range(parent);
    let mut found = false;
    for &child in parent_node.children.iter() {
      let child_node = self.nodes.get(child.index())?;
      if child_node.parent != Some(parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }
      if found {
        return Some(child);
      }
      if child == node {
        found = true;
      }
    }
    None
  }

  /// Delete the contents of a live range.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-deletecontents
  pub fn range_delete_contents(&mut self, range: RangeId) -> DomResult<()> {
    let (start, end) = {
      let r = self.range(range)?;
      (r.start, r.end)
    };
    if start == end {
      return Ok(());
    }

    let (original_start_node, original_start_offset) = (start.node, start.offset);
    let (original_end_node, original_end_offset) = (end.node, end.offset);

    // Fast path: CharacterData-only.
    if original_start_node == original_end_node && self.node_is_character_data_for_range(original_start_node)
    {
      let _ = self.replace_data(
        original_start_node,
        original_start_offset,
        original_end_offset.saturating_sub(original_start_offset),
        "",
      )?;
      // `replace_data` collapses/updates range endpoints via live range maintenance.
      return Ok(());
    }

    // Collect contained nodes whose parent is not also contained.
    let range_root = self.tree_root_for_range(original_start_node);
    let mut nodes_to_remove: Vec<NodeId> = Vec::new();
    let mut stack: Vec<(NodeId, bool)> = vec![(range_root, false)];
    while let Some((node, ancestor_contained)) = stack.pop() {
      let contained = self.is_node_contained_in_range(node, start, end)?;
      if contained {
        if !ancestor_contained {
          nodes_to_remove.push(node);
        }
        // Descendants of a contained node are also contained.
        continue;
      }

      let Some(node_ref) = self.nodes.get(node.index()) else {
        continue;
      };
      let parent_is_element_like = self.parent_is_element_like_for_range(node);
      for &child in node_ref.children.iter().rev() {
        let Some(child_ref) = self.nodes.get(child.index()) else {
          continue;
        };
        if child_ref.parent != Some(node) {
          continue;
        }
        if parent_is_element_like && self.is_shadow_root_node(child) {
          continue;
        }
        stack.push((child, ancestor_contained));
      }
    }

    // Compute where to collapse the range after deletion.
    let collapse_point = if self.is_inclusive_descendant_for_range(original_end_node, original_start_node) {
      BoundaryPoint {
        node: original_start_node,
        offset: original_start_offset,
      }
    } else {
      let mut reference_node = original_start_node;
      let mut remaining = self.nodes.len() + 1;
      while remaining > 0 {
        remaining -= 1;
        let Some(parent) = self.range_parent(reference_node) else {
          break;
        };
        if self.is_inclusive_descendant_for_range(original_end_node, parent) {
          break;
        }
        reference_node = parent;
      }

      if let Some(new_node) = self.range_parent(reference_node) {
        let idx = self.node_index(reference_node).unwrap_or(0);
        BoundaryPoint {
          node: new_node,
          offset: idx.saturating_add(1),
        }
      } else {
        BoundaryPoint {
          node: original_start_node,
          offset: original_start_offset,
        }
      }
    };

    // Mutate the tree.
    if self.node_is_character_data_for_range(original_start_node) {
      let start_len = self.node_length(original_start_node)?;
      let _ = self.replace_data(
        original_start_node,
        original_start_offset,
        start_len.saturating_sub(original_start_offset),
        "",
      )?;
    }

    for node in nodes_to_remove {
      let Some(parent) = self.nodes.get(node.index()).and_then(|n| n.parent) else {
        continue;
      };
      let _ = self.remove_child(parent, node)?;
    }

    if self.node_is_character_data_for_range(original_end_node) {
      let _ = self.replace_data(original_end_node, 0, original_end_offset, "")?;
    }

    let r = self.range_mut(range)?;
    r.start = collapse_point;
    r.end = collapse_point;
    Ok(())
  }

  /// Insert a node into a live range.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-insertnode
  pub fn range_insert_node(&mut self, range: RangeId, node: NodeId) -> DomResult<()> {
    self.node_checked(node)?;
    let start = self.range_start(range)?;
    let start_node = start.node;

    // Track the initial collapsed state (step 13).
    let was_collapsed = start == self.range_end(range)?;

    // Step 1.
    match &self.node_checked(start_node)?.kind {
      NodeKind::ProcessingInstruction { .. } | NodeKind::Comment { .. } => {
        return Err(DomError::HierarchyRequestError);
      }
      NodeKind::Text { .. } => {
        if self.nodes[start_node.index()].parent.is_none() {
          return Err(DomError::HierarchyRequestError);
        }
      }
      _ => {}
    }
    if start_node == node {
      return Err(DomError::HierarchyRequestError);
    }

    // Steps 2–5.
    let mut reference_node = if matches!(&self.node_checked(start_node)?.kind, NodeKind::Text { .. }) {
      Some(start_node)
    } else {
      self.tree_child_at_for_range(start_node, start.offset)
    };

    let parent = match reference_node {
      None => start_node,
      Some(reference_node) => self.range_parent(reference_node).unwrap_or(start_node),
    };

    // Step 6.
    self.ensure_pre_insert_validity(parent, node, reference_node)?;

    // Step 7.
    if matches!(&self.node_checked(start_node)?.kind, NodeKind::Text { .. }) {
      reference_node = Some(self.split_text(start_node, start.offset)?);
    }

    // Step 8.
    if reference_node == Some(node) {
      reference_node = self.range_next_sibling(node);
    }

    // Step 9.
    if let Some(old_parent) = self.nodes.get(node.index()).and_then(|n| n.parent) {
      let _ = self.remove_child(old_parent, node)?;
    }

    // Steps 10–11.
    let mut new_offset = match reference_node {
      None => self.node_length(parent)?,
      Some(reference_node) => self.node_index(reference_node).unwrap_or(self.node_length(parent)?),
    };
    if matches!(
      self.nodes.get(node.index()).map(|n| &n.kind),
      Some(NodeKind::DocumentFragment)
    ) {
      new_offset = new_offset.saturating_add(self.node_length(node)?);
    } else {
      new_offset = new_offset.saturating_add(1);
    }

    // Step 12.
    let _ = self.insert_before(parent, node, reference_node)?;

    // Step 13.
    if was_collapsed {
      let r = self.range_mut(range)?;
      r.end = BoundaryPoint {
        node: parent,
        offset: new_offset,
      };
    }

    Ok(())
  }

  /// Surround the contents of a live range.
  ///
  /// Spec: https://dom.spec.whatwg.org/#dom-range-surroundcontents
  pub fn range_surround_contents(&mut self, range: RangeId, new_parent: NodeId) -> DomResult<()> {
    self.node_checked(new_parent)?;
    let (start, end) = {
      let r = self.range(range)?;
      (r.start, r.end)
    };

    // Step 1: throw if any partially contained node is not Text.
    if start.node != end.node {
      let common_ancestor = self.range_common_ancestor_container(range)?;

      let mut n = start.node;
      while n != common_ancestor {
        if !matches!(self.node_checked(n)?.kind, NodeKind::Text { .. }) {
          return Err(DomError::InvalidStateError);
        }
        n = self.range_parent(n).unwrap_or(common_ancestor);
      }

      let mut n = end.node;
      while n != common_ancestor {
        if !matches!(self.node_checked(n)?.kind, NodeKind::Text { .. }) {
          return Err(DomError::InvalidStateError);
        }
        n = self.range_parent(n).unwrap_or(common_ancestor);
      }
    }

    // Step 2.
    match &self.node_checked(new_parent)?.kind {
      NodeKind::Document { .. }
      | NodeKind::Doctype { .. }
      | NodeKind::DocumentFragment
      | NodeKind::ShadowRoot { .. } => return Err(DomError::InvalidNodeTypeError),
      _ => {}
    }

    // Step 3.
    let fragment = self.range_extract_contents(range)?;

    // Step 4: remove all tree children of newParent.
    let mut children: Vec<NodeId> = Vec::new();
    let parent_is_element_like = self.parent_is_element_like_for_range(new_parent);
    let node = self.node_checked(new_parent)?;
    for &child in node.children.iter() {
      let Some(child_node) = self.nodes.get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(new_parent) {
        continue;
      }
      if parent_is_element_like && self.is_shadow_root_node(child) {
        continue;
      }
      children.push(child);
    }
    for child in children {
      let _ = self.remove_child(new_parent, child)?;
    }

    // Step 5.
    self.range_insert_node(range, new_parent)?;

    // Step 6.
    let _ = self.append_child(new_parent, fragment)?;

    // Step 7.
    self.range_select_node(range, new_parent)?;

    Ok(())
  }

  /// Live range pre-insert steps.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-live-range-pre-insert
  ///
  /// Note: Range boundary point offsets into element-like nodes are defined in terms of DOM "tree
  /// children" (excluding the ShadowRoot pseudo-child stored by `dom2`). Callers are responsible
  /// for passing `index`/`count` in that tree-child coordinate space (see
  /// `tree_child_index_from_raw_index_for_range` and `inserted_tree_children_count_for_range`).
  pub(super) fn live_range_pre_insert_steps(&mut self, parent: NodeId, index: usize, count: usize) {
    if count == 0 || self.ranges.is_empty() {
      return;
    }

    for range in self.ranges.values_mut() {
      if range.start.node == parent && range.start.offset > index {
        range.start.offset = range.start.offset.saturating_add(count);
      }
      if range.end.node == parent && range.end.offset > index {
        range.end.offset = range.end.offset.saturating_add(count);
      }
    }
  }
  /// Live range pre-remove steps.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-live-range-pre-remove
  pub(super) fn live_range_pre_remove_steps(&mut self, node: NodeId, parent: NodeId, index: usize) {
    if self.ranges.is_empty() {
      return;
    }

    // `dom2` stores an attached ShadowRoot as a child of its host element for renderer traversal.
    // That ShadowRoot is not a DOM tree child (`host.childNodes`), so Range live-update indices
    // must ignore it.
    let index = match self.tree_child_index_for_range(parent, node) {
      Some(index) => index,
      None => {
        // Not a tree child. This primarily happens for attached ShadowRoot nodes, which should
        // not affect light-DOM range offsets when attached/detached.
        let is_shadow_root = self
          .nodes
          .get(node.index())
          .is_some_and(|n| matches!(&n.kind, NodeKind::ShadowRoot { .. }));
        let parent_is_element_like = self.nodes.get(parent.index()).is_some_and(|n| {
          matches!(&n.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
        });
        if is_shadow_root && parent_is_element_like {
          return;
        }
        // Defensive fallback: preserve the caller-provided index.
        index
      }
    };

    let boundary_point = BoundaryPoint {
      node: parent,
      offset: index,
    };

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
  /// Updates boundary-point offsets for all live ranges when a `CharacterData` node's data is
  /// replaced via the DOM "replace data" primitive (used by `CharacterData.deleteData`,
  /// `CharacterData.replaceData`, and the `data` setter).
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-cd-replace (steps 8–11).
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
    for range in self.ranges.values_mut() {
      if range.start.node == node && range.start.offset > offset && range.start.offset <= end {
        range.start.offset = offset;
      }
    }

    // 9. For each live range whose end node is node and end offset is greater than offset but less
    // than or equal to offset + count: set its end offset to offset.
    for range in self.ranges.values_mut() {
      if range.end.node == node && range.end.offset > offset && range.end.offset <= end {
        range.end.offset = offset;
      }
    }

    // 10/11. For each live range whose start/end offset is greater than offset + count: increase
    // by data's length and decrease by count.
    if inserted_len >= removed_len {
      let delta = inserted_len - removed_len;
      if delta != 0 {
        for range in self.ranges.values_mut() {
          if range.start.node == node && range.start.offset > end {
            range.start.offset = range.start.offset.saturating_add(delta);
          }
        }
        for range in self.ranges.values_mut() {
          if range.end.node == node && range.end.offset > end {
            range.end.offset = range.end.offset.saturating_add(delta);
          }
        }
      }
    } else {
      let delta = removed_len - inserted_len;
      for range in self.ranges.values_mut() {
        if range.start.node == node && range.start.offset > end {
          range.start.offset = range.start.offset.saturating_sub(delta);
        }
      }
      for range in self.ranges.values_mut() {
        if range.end.node == node && range.end.offset > end {
          range.end.offset = range.end.offset.saturating_sub(delta);
        }
      }
    }
  }

  /// Live range updates for merging a `Text` node into another and removing the merged-away node.
  ///
  /// `from` is the text node that will be removed. `to` is the surviving text node that now
  /// contains `from`'s data. `offset` is the UTF-16 code unit offset in `to` where `from`'s data is
  /// inserted (i.e. the length of `to`'s data *before* the merge point).
  ///
  /// This mirrors the relevant `Range` maintenance behavior from DOM's `Node.normalize()` and other
  /// text-node-merge algorithms: boundary points that were inside the removed node are moved into
  /// the surviving node with their offsets shifted by `offset`.
  pub(super) fn live_range_merge_text_steps(&mut self, from: NodeId, to: NodeId, offset: usize) {
    if self.ranges.is_empty() {
      return;
    }
    if from == to {
      return;
    }

    for range in self.ranges.values_mut() {
      if range.start.node == from {
        range.start.node = to;
        range.start.offset = range.start.offset.saturating_add(offset);
      }
      if range.end.node == from {
        range.end.node = to;
        range.end.offset = range.end.offset.saturating_add(offset);
      }
    }
  }

  /// Live range updates for the DOM `Text.splitText()` algorithm.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-text-split
  ///
  /// Note: this only implements the splitText-specific range adjustments (moving boundary points
  /// from the old text node into the newly-created node, plus shifting boundary points at the
  /// parent immediately after the split node). Generic insert/replace-data live range maintenance
  /// is handled by their respective mutation primitives.
  pub(super) fn live_range_split_text_steps(
    &mut self,
    node: NodeId,
    offset: usize,
    new_node: NodeId,
    parent: NodeId,
    index: usize,
  ) {
    // 7.2: Move start boundary points that were inside the split tail.
    for range in self.ranges.values_mut() {
      if range.start.node == node && range.start.offset > offset {
        range.start.node = new_node;
        range.start.offset -= offset;
      }
    }

    // 7.3: Move end boundary points that were inside the split tail.
    for range in self.ranges.values_mut() {
      if range.end.node == node && range.end.offset > offset {
        range.end.node = new_node;
        range.end.offset -= offset;
      }
    }

    // The new node was inserted immediately after `node`, so it sits at `index + 1`.
    //
    // `index` is the split node's index in the parent *tree child* list (ShadowRoot-aware), which
    // matches Range boundary-point offset semantics.
    let insertion_offset = index.saturating_add(1);

    // 7.4: Shift parent start boundary points that were immediately after the split node.
    for range in self.ranges.values_mut() {
      if range.start.node == parent && range.start.offset == insertion_offset {
        range.start.offset = range.start.offset.saturating_add(1);
      }
    }

    // 7.5: Shift parent end boundary points that were immediately after the split node.
    for range in self.ranges.values_mut() {
      if range.end.node == parent && range.end.offset == insertion_offset {
        range.end.offset = range.end.offset.saturating_add(1);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::super::parse_html;

  #[test]
  fn substring_character_data_for_range_uses_code_unit_offsets() {
    // 😀 is a single Unicode scalar value but encoded as a surrogate pair in UTF-16.
    let mut doc: super::Document = parse_html("<!doctype html><html></html>").unwrap();
    let text = doc.create_text("x😀y");

    assert_eq!(
      doc.substring_character_data_for_range(text, 0, 1).unwrap(),
      "x"
    );
    assert_eq!(
      doc.substring_character_data_for_range(text, 1, 2).unwrap(),
      "😀"
    );
    assert_eq!(
      doc.substring_character_data_for_range(text, 3, 1).unwrap(),
      "y"
    );
  }
}
