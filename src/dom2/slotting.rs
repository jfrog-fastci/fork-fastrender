use rustc_hash::{FxHashMap, FxHashSet};

use crate::dom::ShadowRootMode;

use super::{Attribute, Document, DomError, NodeId, NodeKind, NULL_NAMESPACE};

/// ShadowRoot slot assignment mode.
///
/// Mirrors the HTML `SlotAssignmentMode` enum (`"named"` | `"manual"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotAssignmentMode {
  Named,
  Manual,
}

impl Default for SlotAssignmentMode {
  fn default() -> Self {
    Self::Named
  }
}

impl SlotAssignmentMode {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Named => "named",
      Self::Manual => "manual",
    }
  }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SlottingState {
  /// Per-slottable "manual slot assignment" (null or a slot element).
  pub(crate) manual_slot_for_slottable: FxHashMap<NodeId, NodeId>,
  /// Per-slot "manually assigned nodes" ordered set.
  pub(crate) manual_assigned_nodes_for_slot: FxHashMap<NodeId, Vec<NodeId>>,
  /// Per-slot assigned nodes, as computed by the slotting algorithms.
  pub(crate) assigned_nodes_for_slot: FxHashMap<NodeId, Vec<NodeId>>,
  /// Per-slottable assigned slot, as computed by the slotting algorithms.
  pub(crate) assigned_slot_for_slottable: FxHashMap<NodeId, NodeId>,
}

fn get_attribute_value<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
  attrs
    .iter()
    .find(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
    .map(|attr| attr.value.as_str())
}

fn slot_name<'a>(kind: &'a NodeKind) -> Option<&'a str> {
  let NodeKind::Slot { attributes, .. } = kind else {
    return None;
  };
  Some(get_attribute_value(attributes, "name").unwrap_or(""))
}

fn slottable_slot_attr<'a>(kind: &'a NodeKind) -> Option<&'a str> {
  let attrs = match kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
    _ => return Some(""),
  };
  Some(get_attribute_value(attrs, "slot").unwrap_or(""))
}

fn node_is_slottable(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. } | NodeKind::Text { .. })
}

fn shadow_root_slot_assignment(kind: &NodeKind) -> Option<SlotAssignmentMode> {
  match kind {
    NodeKind::ShadowRoot { slot_assignment, .. } => Some(*slot_assignment),
    _ => None,
  }
}

impl Document {
  pub(crate) fn slotting(&self) -> &SlottingState {
    &self.slotting
  }

  pub(crate) fn slotting_mut(&mut self) -> &mut SlottingState {
    &mut self.slotting
  }

  /// Returns the shadow root child of `host`, if present.
  pub fn shadow_root_for_host(&self, host: NodeId) -> Option<NodeId> {
    let node = self.nodes.get(host.index())?;
    for &child in &node.children {
      let child_node = self.nodes.get(child.index())?;
      if child_node.parent != Some(host) {
        continue;
      }
      if matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
        return Some(child);
      }
    }
    None
  }

  /// Find the nearest inclusive ancestor shadow root for `node`.
  pub fn shadow_root_ancestor(&self, mut node: NodeId) -> Option<NodeId> {
    loop {
      if matches!(self.node(node).kind, NodeKind::ShadowRoot { .. }) {
        return Some(node);
      }
      node = self.node(node).parent?;
    }
  }

  /// Implements HTML's "find a slot" algorithm.
  ///
  /// Returns the assigned slot element for `slottable`, if any.
  pub fn find_slot_for_slottable(&self, slottable: NodeId, open: bool) -> Option<NodeId> {
    let slottable_node = self.nodes.get(slottable.index())?;
    if !node_is_slottable(&slottable_node.kind) {
      return None;
    }

    let host = slottable_node.parent?;
    let shadow_root = self.shadow_root_for_host(host)?;
    if open {
      let NodeKind::ShadowRoot { mode, .. } = &self.node(shadow_root).kind else {
        return None;
      };
      if *mode == ShadowRootMode::Closed {
        return None;
      }
    }
    let mode = shadow_root_slot_assignment(&self.node(shadow_root).kind).unwrap_or_default();
    match mode {
      SlotAssignmentMode::Manual => {
        let slot = self.slotting.manual_slot_for_slottable.get(&slottable).copied()?;
        // Manual assignment only applies to slottables that are children of the host, and only when
        // the assigned slot is in the same shadow tree.
        if self.node(slottable).parent != Some(host) {
          return None;
        }
        if self.shadow_root_ancestor(slot) != Some(shadow_root) {
          return None;
        }
        Some(slot)
      }
      SlotAssignmentMode::Named => {
        let name = slottable_slot_attr(&slottable_node.kind).unwrap_or("");
        self.first_slot_in_shadow_tree_with_name(shadow_root, name)
      }
    }
  }

  fn first_slot_in_shadow_tree_with_name(&self, shadow_root: NodeId, name: &str) -> Option<NodeId> {
    // Pre-order traverse the shadow tree to find the first <slot> element with a matching name.
    let mut stack: Vec<NodeId> = Vec::new();
    stack.push(shadow_root);
    while let Some(id) = stack.pop() {
      let node = self.nodes.get(id.index())?;
      if id != shadow_root && matches!(node.kind, NodeKind::ShadowRoot { .. }) {
        // Nested shadow roots are separate trees; do not traverse into their descendants when
        // looking for slots in `shadow_root`'s shadow tree.
        continue;
      }
      if matches!(node.kind, NodeKind::Slot { .. }) {
        let slot_name = slot_name(&node.kind).unwrap_or("");
        if slot_name == name {
          return Some(id);
        }
      }
      for &child in node.children.iter().rev() {
        let child_node = self.nodes.get(child.index())?;
        if child_node.parent == Some(id) {
          stack.push(child);
        }
      }
    }
    None
  }

  /// Implements HTML's "find slottables" algorithm.
  pub fn find_slottables_for_slot(&self, slot: NodeId) -> Vec<NodeId> {
    if !matches!(self.node(slot).kind, NodeKind::Slot { .. }) {
      return Vec::new();
    }

    let mut out: Vec<NodeId> = Vec::new();

    let shadow_root = self.shadow_root_ancestor(slot);
    if let Some(shadow_root) = shadow_root {
      let mode = shadow_root_slot_assignment(&self.node(shadow_root).kind).unwrap_or_default();
      if let Some(host) = self.node(shadow_root).parent {
        match mode {
          SlotAssignmentMode::Manual => {
            if let Some(list) = self.slotting.manual_assigned_nodes_for_slot.get(&slot) {
              for &node in list {
                if node.index() >= self.nodes.len() {
                  continue;
                }
                if self.node(node).parent != Some(host) {
                  continue;
                }
                if !node_is_slottable(&self.node(node).kind) {
                  continue;
                }
                out.push(node);
              }
            }
          }
          SlotAssignmentMode::Named => {
            let name = slot_name(&self.node(slot).kind).unwrap_or("");
            let first = self.first_slot_in_shadow_tree_with_name(shadow_root, name);
            if first == Some(slot) {
              for &child in &self.node(host).children {
                if child.index() >= self.nodes.len() {
                  continue;
                }
                if self.node(child).parent != Some(host) {
                  continue;
                }
                if !node_is_slottable(&self.node(child).kind) {
                  continue;
                }
                let child_name = slottable_slot_attr(&self.node(child).kind).unwrap_or("");
                if child_name == name {
                  out.push(child);
                }
              }
            }
          }
        }
      }
    }

    if !out.is_empty() {
      return out;
    }

    // Fallback content: if there are no assigned slottables, return the slot's children.
    self
      .node(slot)
      .children
      .iter()
      .copied()
      .filter(|&child| child.index() < self.nodes.len() && self.node(child).parent == Some(slot))
      .collect()
  }

  /// Implements HTML's "find flattened slottables" algorithm.
  pub fn find_flattened_slottables_for_slot(&self, slot: NodeId) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    let mut visited: FxHashSet<NodeId> = FxHashSet::default();
    self.append_flattened_slottables(slot, &mut visited, &mut out);
    out
  }

  fn append_flattened_slottables(
    &self,
    slot: NodeId,
    visited: &mut FxHashSet<NodeId>,
    out: &mut Vec<NodeId>,
  ) {
    if !visited.insert(slot) {
      return;
    }
    let slottables = self.find_slottables_for_slot(slot);
    for node in slottables {
      if matches!(self.node(node).kind, NodeKind::Slot { .. }) {
        self.append_flattened_slottables(node, visited, out);
      } else {
        out.push(node);
      }
    }
  }

  /// Set the manual slot assignments for `slot`.
  ///
  /// Mirrors the HTML `HTMLSlotElement.assign(..nodes)` algorithm:
  /// - stores per-slot "manually assigned nodes" (ordered set), and
  /// - stores per-slottable "manual slot assignment" (slot or null).
  pub fn slot_assign(&mut self, slot: NodeId, nodes: &[NodeId]) -> Result<(), DomError> {
    self.node_checked(slot)?;
    if !matches!(self.node(slot).kind, NodeKind::Slot { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }

    // 1. For each node in this slot's current manually assigned nodes: clear its manual assignment.
    if let Some(prev) = self.slotting.manual_assigned_nodes_for_slot.remove(&slot) {
      for node in prev {
        if self.slotting.manual_slot_for_slottable.get(&node) == Some(&slot) {
          self.slotting.manual_slot_for_slottable.remove(&node);
        }
      }
    }

    // 2. Set this slot's manually assigned nodes to the ordered set of new slottables.
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut new_list: Vec<NodeId> = Vec::new();

    for &node in nodes {
      self.node_checked(node)?;
      if !node_is_slottable(&self.node(node).kind) {
        return Err(DomError::InvalidNodeTypeError);
      }

      if !seen.insert(node) {
        continue;
      }

      if let Some(old_slot) = self.slotting.manual_slot_for_slottable.insert(node, slot) {
        if old_slot != slot {
          if let Some(list) = self.slotting.manual_assigned_nodes_for_slot.get_mut(&old_slot) {
            list.retain(|&n| n != node);
          }
        }
      }

      new_list.push(node);
    }

    self
      .slotting
      .manual_assigned_nodes_for_slot
      .insert(slot, new_list);

    // Manual assignments affect slot distribution/composed tree; treat as a render-affecting mutation.
    self.bump_mutation_generation_classified();
    // Slot assignment changes the composed tree even when the DOM tree structure is unchanged.
    //
    // Record a structured mutation so `BrowserDocumentDom2` can invalidate without falling back to
    // `invalidate_all()` (which would discard the reason for the change).
    if let Some(shadow_root) = self.shadow_root_ancestor(slot) {
      self.record_composed_tree_mutation(shadow_root);
    } else {
      self.record_composed_tree_mutation(slot);
    }

    // Run "assign slottables for a tree" for this slot's root (HTML).
    if let Some(shadow_root) = self.shadow_root_ancestor(slot) {
      self.assign_slottables_for_tree(shadow_root);
    }

    Ok(())
  }

  /// Compute slot distribution for a shadow root and update derived assignment state.
  ///
  /// This corresponds to the HTML "assign slottables for a tree" algorithm.
  pub(crate) fn assign_slottables_for_tree(&mut self, shadow_root: NodeId) {
    if !matches!(self.node(shadow_root).kind, NodeKind::ShadowRoot { .. }) {
      return;
    }
    let Some(host) = self.node(shadow_root).parent else {
      return;
    };

    // Collect slots in tree order (skipping nested shadow roots).
    let mut slots: Vec<NodeId> = Vec::new();
    let mut stack: Vec<NodeId> = Vec::new();
    stack.push(shadow_root);
    while let Some(id) = stack.pop() {
      if id != shadow_root && matches!(self.node(id).kind, NodeKind::ShadowRoot { .. }) {
        // Shadow roots are separate trees; do not traverse into nested shadow trees.
        continue;
      }
      if matches!(self.node(id).kind, NodeKind::Slot { .. }) {
        slots.push(id);
      }
      // Push children in reverse so we traverse in tree order.
      let children = self.node(id).children.clone();
      for child in children.into_iter().rev() {
        if child.index() >= self.nodes.len() {
          continue;
        }
        if self.node(child).parent == Some(id) {
          stack.push(child);
        }
      }
    }

    // Clear previous derived assignment state for these slots.
    for &slot in &slots {
      if let Some(old) = self.slotting.assigned_nodes_for_slot.remove(&slot) {
        for node in old {
          if self.slotting.assigned_slot_for_slottable.get(&node) == Some(&slot) {
            self.slotting.assigned_slot_for_slottable.remove(&node);
          }
        }
      }
      // Ensure an entry exists for `assignedNodes()`.
      self.slotting.assigned_nodes_for_slot.insert(slot, Vec::new());
      if let NodeKind::Slot { assigned, .. } = &mut self.nodes[slot.index()].kind {
        *assigned = false;
      }
    }

    let mode = shadow_root_slot_assignment(&self.node(shadow_root).kind).unwrap_or_default();
    match mode {
      SlotAssignmentMode::Manual => {
        let mut seen: FxHashSet<NodeId> = FxHashSet::default();
        for &slot in &slots {
          let Some(list) = self.slotting.manual_assigned_nodes_for_slot.get(&slot) else {
            continue;
          };
          for &node in list {
            if !seen.insert(node) {
              continue;
            }
            if node.index() >= self.nodes.len() {
              continue;
            }
            if self.node(node).parent != Some(host) {
              continue;
            }
            if !node_is_slottable(&self.node(node).kind) {
              continue;
            }
            self
              .slotting
              .assigned_slot_for_slottable
              .insert(node, slot);
            if let Some(vec) = self.slotting.assigned_nodes_for_slot.get_mut(&slot) {
              vec.push(node);
            }
          }
        }
      }
      SlotAssignmentMode::Named => {
        let mut first_slot_for_name: FxHashMap<String, NodeId> = FxHashMap::default();
        for &slot in &slots {
          let name = slot_name(&self.node(slot).kind).unwrap_or("");
          first_slot_for_name.entry(name.to_string()).or_insert(slot);
        }

        let host_children = self.node(host).children.clone();
        for child in host_children {
          if child.index() >= self.nodes.len() {
            continue;
          }
          if self.node(child).parent != Some(host) {
            continue;
          }
          if !node_is_slottable(&self.node(child).kind) {
            continue;
          }

          let name = slottable_slot_attr(&self.node(child).kind).unwrap_or("");
          let Some(&slot) = first_slot_for_name.get(name) else {
            continue;
          };

          self
            .slotting
            .assigned_slot_for_slottable
            .insert(child, slot);
          if let Some(vec) = self.slotting.assigned_nodes_for_slot.get_mut(&slot) {
            vec.push(child);
          }
        }
      }
    }

    // Update each slot's derived `assigned` flag.
    for &slot in &slots {
      let assigned = self
        .slotting
        .assigned_nodes_for_slot
        .get(&slot)
        .is_some_and(|list| !list.is_empty());
      if let NodeKind::Slot { assigned: flag, .. } = &mut self.nodes[slot.index()].kind {
        *flag = assigned;
      }
    }
  }
}
