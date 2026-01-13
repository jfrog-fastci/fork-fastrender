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

  pub(crate) fn node_length(&self, node: NodeId) -> DomResult<usize> {
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
      _ => Err(DomError::InvalidNodeType),
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
      _ => Err(DomError::InvalidNodeType),
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

    let common_children: Vec<NodeId> = self.nodes[common_ancestor.index()].children.clone();

    let first_partially_contained_child = if original_start_is_inclusive_ancestor_of_end {
      None
    } else {
      common_children
        .iter()
        .copied()
        .find(|&child| {
          self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor))
            && self.is_node_partially_contained_in_range(child, start_node, end_node)
        })
    };

    let last_partially_contained_child = if original_end_is_inclusive_ancestor_of_start {
      None
    } else {
      common_children
        .iter()
        .rev()
        .copied()
        .find(|&child| {
          self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor))
            && self.is_node_partially_contained_in_range(child, start_node, end_node)
        })
    };

    let mut contained_children: Vec<NodeId> = Vec::new();
    for child in common_children.iter().copied() {
      if !self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor)) {
        continue;
      }
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

    let common_children: Vec<NodeId> = self.nodes[common_ancestor.index()].children.clone();

    let first_partially_contained_child = if original_start_is_inclusive_ancestor_of_end {
      None
    } else {
      common_children
        .iter()
        .copied()
        .find(|&child| {
          self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor))
            && self.is_node_partially_contained_in_range(child, start_node, end_node)
        })
    };

    let last_partially_contained_child = if original_end_is_inclusive_ancestor_of_start {
      None
    } else {
      common_children
        .iter()
        .rev()
        .copied()
        .find(|&child| {
          self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor))
            && self.is_node_partially_contained_in_range(child, start_node, end_node)
        })
    };

    let mut contained_children: Vec<NodeId> = Vec::new();
    for child in common_children.iter().copied() {
      if !self.nodes.get(child.index()).is_some_and(|n| n.parent == Some(common_ancestor)) {
        continue;
      }
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
