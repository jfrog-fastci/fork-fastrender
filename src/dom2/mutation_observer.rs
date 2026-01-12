use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, NodeId, NodeKind};
use std::collections::{HashMap, HashSet};

pub type MutationObserverId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationRecordType {
  Attributes,
  CharacterData,
  ChildList,
}

impl MutationRecordType {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Attributes => "attributes",
      Self::CharacterData => "characterData",
      Self::ChildList => "childList",
    }
  }
}

#[derive(Debug, Clone)]
pub struct MutationRecord {
  pub type_: MutationRecordType,
  pub target: NodeId,
  pub added_nodes: Vec<NodeId>,
  pub removed_nodes: Vec<NodeId>,
  pub previous_sibling: Option<NodeId>,
  pub next_sibling: Option<NodeId>,
  pub attribute_name: Option<String>,
  pub old_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationObserverInit {
  pub child_list: bool,
  pub attributes: bool,
  pub character_data: bool,
  pub subtree: bool,
  pub attribute_old_value: bool,
  pub character_data_old_value: bool,
  pub attribute_filter: Option<Vec<String>>,
}

impl Default for MutationObserverInit {
  fn default() -> Self {
    Self {
      child_list: false,
      attributes: false,
      character_data: false,
      subtree: false,
      attribute_old_value: false,
      character_data_old_value: false,
      attribute_filter: None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationObserverLimits {
  pub max_observers: usize,
  pub max_records_per_observer: usize,
  pub max_total_records: usize,
}

impl Default for MutationObserverLimits {
  fn default() -> Self {
    Self {
      max_observers: 10_000,
      max_records_per_observer: 10_000,
      max_total_records: 100_000,
    }
  }
}

#[derive(Debug, Clone)]
pub(crate) struct MutationObserverRegistry {
  limits: MutationObserverLimits,
  observers: HashMap<MutationObserverId, ObserverState>,
  /// Per-node registered observers (indexed by `NodeId.index()`).
  registrations: Vec<Vec<Registration>>,
  pending: Vec<MutationObserverId>,
  microtask_queued: bool,
  microtask_needs_queueing: bool,
  total_records: usize,
}

#[derive(Debug, Clone)]
struct ObserverState {
  records: Vec<MutationRecord>,
  observed_targets: Vec<NodeId>,
  in_pending: bool,
}

#[derive(Debug, Clone)]
struct Registration {
  observer: MutationObserverId,
  options: MutationObserverInit,
}

impl MutationObserverRegistry {
  pub(crate) fn new(nodes_len: usize) -> Self {
    Self {
      limits: MutationObserverLimits::default(),
      observers: HashMap::new(),
      registrations: vec![Vec::new(); nodes_len],
      pending: Vec::new(),
      microtask_queued: false,
      microtask_needs_queueing: false,
      total_records: 0,
    }
  }

  pub(crate) fn on_node_added(&mut self) {
    self.registrations.push(Vec::new());
  }

  pub(crate) fn limits(&self) -> MutationObserverLimits {
    self.limits
  }

  pub(crate) fn set_limits(&mut self, limits: MutationObserverLimits) {
    self.limits = limits;
  }

  /// Remap internal [`NodeId`] references after a DOM subtree has been adopted/imported via
  /// clone+mapping.
  ///
  /// Some host integrations preserve JS identity by updating wrapper objects to point at new
  /// `dom2::NodeId`s. Mutation observer state stores `NodeId`s in:
  /// - each observer's observed target list
  /// - queued mutation records
  ///
  /// This helper updates those references so subsequent deliveries return the correct JS nodes.
  ///
  /// Entries absent from `mapping` are left unchanged. Observed target lists are deduplicated after
  /// remapping (preserving order).
  pub(crate) fn remap_node_ids(&mut self, mapping: &HashMap<NodeId, NodeId>) {
    for state in self.observers.values_mut() {
      if !state.observed_targets.is_empty() {
        let mut deduped: Vec<NodeId> = Vec::with_capacity(state.observed_targets.len());
        let mut seen: HashSet<NodeId> = HashSet::with_capacity(state.observed_targets.len());
        for &id in &state.observed_targets {
          let remapped = mapping.get(&id).copied().unwrap_or(id);
          if seen.insert(remapped) {
            deduped.push(remapped);
          }
        }
        state.observed_targets = deduped;
      }

      for record in &mut state.records {
        if let Some(&new_target) = mapping.get(&record.target) {
          record.target = new_target;
        }
        for node in &mut record.added_nodes {
          if let Some(&new_node) = mapping.get(node) {
            *node = new_node;
          }
        }
        for node in &mut record.removed_nodes {
          if let Some(&new_node) = mapping.get(node) {
            *node = new_node;
          }
        }
        if let Some(prev) = record.previous_sibling {
          if let Some(&new_prev) = mapping.get(&prev) {
            record.previous_sibling = Some(new_prev);
          }
        }
        if let Some(next) = record.next_sibling {
          if let Some(&new_next) = mapping.get(&next) {
            record.next_sibling = Some(new_next);
          }
        }
      }
    }
  }

  /// Move per-node observer registrations from `old` to `new`.
  ///
  /// Mutation observer registrations are stored separately from `Node` in `dom2`. When a node is
  /// adopted/imported by cloning and remapping wrapper identities, the embedding layer should move
  /// registrations to the new node so future DOM mutations continue to queue records.
  pub(crate) fn move_registrations(&mut self, old: NodeId, new: NodeId) {
    if old == new {
      return;
    }
    let old_idx = old.index();
    let new_idx = new.index();
    if old_idx >= self.registrations.len() || new_idx >= self.registrations.len() {
      return;
    }
    if old_idx == new_idx {
      return;
    }

    let (old_list, new_list) = if old_idx < new_idx {
      let (left, right) = self.registrations.split_at_mut(new_idx);
      (&mut left[old_idx], &mut right[0])
    } else {
      let (left, right) = self.registrations.split_at_mut(old_idx);
      (&mut right[0], &mut left[new_idx])
    };

    if old_list.is_empty() {
      return;
    }

    let moved = std::mem::take(old_list);
    for reg in moved {
      new_list.retain(|r| r.observer != reg.observer);
      new_list.push(reg);
    }
  }

  fn state_for_observer_mut(
    &mut self,
    observer: MutationObserverId,
  ) -> Result<&mut ObserverState, DomError> {
    let exists = self.observers.contains_key(&observer);
    if !exists && self.observers.len() >= self.limits.max_observers {
      return Err(DomError::NotSupportedError);
    }
    Ok(
      self
        .observers
        .entry(observer)
        .or_insert_with(|| ObserverState {
          records: Vec::new(),
          observed_targets: Vec::new(),
          in_pending: false,
        }),
    )
  }

  fn queue_record(
    &mut self,
    observer: MutationObserverId,
    record: MutationRecord,
  ) -> Result<(), DomError> {
    let limits = self.limits;
    if self.total_records >= limits.max_total_records {
      return Ok(());
    }

    let observers_len = self.observers.len();
    let state = match self.observers.entry(observer) {
      std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
      std::collections::hash_map::Entry::Vacant(entry) => {
        if observers_len >= limits.max_observers {
          return Err(DomError::NotSupportedError);
        }
        entry.insert(ObserverState {
          records: Vec::new(),
          observed_targets: Vec::new(),
          in_pending: false,
        })
      }
    };

    if state.records.len() >= limits.max_records_per_observer {
      return Ok(());
    }

    state.records.push(record);
    self.total_records = self.total_records.saturating_add(1);

    if !state.in_pending {
      state.in_pending = true;
      self.pending.push(observer);
    }

    if !self.microtask_queued {
      self.microtask_queued = true;
      self.microtask_needs_queueing = true;
    }

    Ok(())
  }
}

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn is_html_element_kind(kind: &NodeKind) -> bool {
  match kind {
    NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
      is_html_namespace(namespace)
    }
    _ => false,
  }
}

impl Document {
  pub fn mutation_observer_limits(&self) -> MutationObserverLimits {
    self.mutation_observers.limits()
  }

  pub fn set_mutation_observer_limits(&mut self, limits: MutationObserverLimits) {
    self.mutation_observers.set_limits(limits);
  }

  pub fn take_mutation_observer_microtask_needed(&mut self) -> bool {
    std::mem::take(&mut self.mutation_observers.microtask_needs_queueing)
  }

  pub fn mutation_observer_observe(
    &mut self,
    observer: MutationObserverId,
    target: NodeId,
    options: MutationObserverInit,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    // Remove any existing registration for (target, observer).
    if let Some(existing) = self
      .mutation_observers
      .registrations
      .get_mut(target.index())
    {
      existing.retain(|reg| reg.observer != observer);
      existing.push(Registration {
        observer,
        options: options.clone(),
      });
    }

    let state = self.mutation_observers.state_for_observer_mut(observer)?;
    if !state.observed_targets.contains(&target) {
      state.observed_targets.push(target);
    }

    Ok(())
  }

  pub fn mutation_observer_disconnect(&mut self, observer: MutationObserverId) {
    let Some(state) = self.mutation_observers.observers.remove(&observer) else {
      return;
    };

    self.mutation_observers.total_records = self
      .mutation_observers
      .total_records
      .saturating_sub(state.records.len());

    if state.in_pending {
      self.mutation_observers.pending.retain(|&id| id != observer);
    }

    for target in state.observed_targets {
      if let Some(list) = self
        .mutation_observers
        .registrations
        .get_mut(target.index())
      {
        list.retain(|reg| reg.observer != observer);
      }
    }
  }

  pub fn mutation_observer_take_records(
    &mut self,
    observer: MutationObserverId,
  ) -> Vec<MutationRecord> {
    let Some(state) = self.mutation_observers.observers.get_mut(&observer) else {
      return Vec::new();
    };
    self.mutation_observers.total_records = self
      .mutation_observers
      .total_records
      .saturating_sub(state.records.len());
    std::mem::take(&mut state.records)
  }

  pub fn mutation_observer_take_deliveries(
    &mut self,
  ) -> Vec<(MutationObserverId, Vec<MutationRecord>)> {
    self.mutation_observers.microtask_queued = false;
    self.mutation_observers.microtask_needs_queueing = false;

    let pending = std::mem::take(&mut self.mutation_observers.pending);
    let mut out: Vec<(MutationObserverId, Vec<MutationRecord>)> = Vec::new();
    for observer in pending {
      let Some(state) = self.mutation_observers.observers.get_mut(&observer) else {
        continue;
      };
      state.in_pending = false;
      if state.records.is_empty() {
        continue;
      }
      self.mutation_observers.total_records = self
        .mutation_observers
        .total_records
        .saturating_sub(state.records.len());
      out.push((observer, std::mem::take(&mut state.records)));
    }
    out
  }

  pub(crate) fn queue_mutation_record_attributes(
    &mut self,
    target: NodeId,
    name: &str,
    old_value: Option<String>,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    let is_html = is_html_element_kind(&self.nodes[target.index()].kind);
    let attr_name = if is_html {
      name.to_ascii_lowercase()
    } else {
      name.to_string()
    };

    let mut interested: HashMap<MutationObserverId, MutationObserverInit> = HashMap::new();
    let mut current = Some(target);
    while let Some(node) = current {
      if let Some(list) = self.mutation_observers.registrations.get(node.index()) {
        for reg in list {
          if !reg.options.attributes {
            continue;
          }
          if node != target && !reg.options.subtree {
            continue;
          }
          if let Some(filter) = reg.options.attribute_filter.as_ref() {
            // `attributeFilter` matching is case-sensitive; HTML attribute names are already
            // normalized to ASCII lowercase before reaching this stage, mirroring the DOM Standard's
            // `localName` normalization.
            let matches = filter.iter().any(|f| f == &attr_name);
            if !matches {
              continue;
            }
          }
          interested
            .entry(reg.observer)
            .or_insert_with(|| reg.options.clone());
        }
      }
      current = self.nodes[node.index()].parent;
    }

    for (observer, options) in interested {
      let record = MutationRecord {
        type_: MutationRecordType::Attributes,
        target,
        added_nodes: Vec::new(),
        removed_nodes: Vec::new(),
        previous_sibling: None,
        next_sibling: None,
        attribute_name: Some(attr_name.clone()),
        old_value: if options.attribute_old_value {
          old_value.clone()
        } else {
          None
        },
      };
      self.mutation_observers.queue_record(observer, record)?;
    }

    Ok(())
  }

  pub(crate) fn queue_mutation_record_character_data(
    &mut self,
    target: NodeId,
    old_value: Option<String>,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    let mut interested: HashMap<MutationObserverId, MutationObserverInit> = HashMap::new();
    let mut current = Some(target);
    while let Some(node) = current {
      if let Some(list) = self.mutation_observers.registrations.get(node.index()) {
        for reg in list {
          if !reg.options.character_data {
            continue;
          }
          if node != target && !reg.options.subtree {
            continue;
          }
          interested
            .entry(reg.observer)
            .or_insert_with(|| reg.options.clone());
        }
      }
      current = self.nodes[node.index()].parent;
    }

    for (observer, options) in interested {
      let record = MutationRecord {
        type_: MutationRecordType::CharacterData,
        target,
        added_nodes: Vec::new(),
        removed_nodes: Vec::new(),
        previous_sibling: None,
        next_sibling: None,
        attribute_name: None,
        old_value: if options.character_data_old_value {
          old_value.clone()
        } else {
          None
        },
      };
      self.mutation_observers.queue_record(observer, record)?;
    }

    Ok(())
  }

  pub(crate) fn queue_mutation_record_child_list(
    &mut self,
    target: NodeId,
    added_nodes: Vec<NodeId>,
    removed_nodes: Vec<NodeId>,
    previous_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    let mut interested: HashMap<MutationObserverId, MutationObserverInit> = HashMap::new();
    let mut current = Some(target);
    while let Some(node) = current {
      if let Some(list) = self.mutation_observers.registrations.get(node.index()) {
        for reg in list {
          if !reg.options.child_list {
            continue;
          }
          if node != target && !reg.options.subtree {
            continue;
          }
          interested
            .entry(reg.observer)
            .or_insert_with(|| reg.options.clone());
        }
      }
      current = self.nodes[node.index()].parent;
    }

    if interested.is_empty() {
      return Ok(());
    }

    for (observer, _options) in interested {
      let record = MutationRecord {
        type_: MutationRecordType::ChildList,
        target,
        added_nodes: added_nodes.clone(),
        removed_nodes: removed_nodes.clone(),
        previous_sibling,
        next_sibling,
        attribute_name: None,
        old_value: None,
      };
      self.mutation_observers.queue_record(observer, record)?;
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn record(target: NodeId) -> MutationRecord {
    MutationRecord {
      type_: MutationRecordType::Attributes,
      target,
      added_nodes: Vec::new(),
      removed_nodes: Vec::new(),
      previous_sibling: None,
      next_sibling: None,
      attribute_name: Some("id".to_string()),
      old_value: None,
    }
  }

  #[test]
  fn queue_record_creates_state_and_schedules_delivery() {
    let mut registry = MutationObserverRegistry::new(1);
    registry.queue_record(1, record(NodeId::from_index(0))).unwrap();

    assert!(registry.microtask_queued);
    assert!(registry.microtask_needs_queueing);
    assert_eq!(registry.pending, vec![1]);
    assert_eq!(registry.total_records, 1);

    let state = registry.observers.get(&1).unwrap();
    assert_eq!(state.records.len(), 1);
    assert!(state.in_pending);
  }

  #[test]
  fn queue_record_is_bounded_per_observer() {
    let mut registry = MutationObserverRegistry::new(1);
    registry.set_limits(MutationObserverLimits {
      max_observers: 10,
      max_records_per_observer: 1,
      max_total_records: 10,
    });

    registry.queue_record(1, record(NodeId::from_index(0))).unwrap();
    registry.queue_record(1, record(NodeId::from_index(0))).unwrap();

    assert_eq!(registry.total_records, 1);
    assert_eq!(registry.pending, vec![1]);
    assert_eq!(registry.observers.get(&1).unwrap().records.len(), 1);
  }

  #[test]
  fn queue_record_is_bounded_globally() {
    let mut registry = MutationObserverRegistry::new(1);
    registry.set_limits(MutationObserverLimits {
      max_observers: 10,
      max_records_per_observer: 10,
      max_total_records: 1,
    });

    registry.queue_record(1, record(NodeId::from_index(0))).unwrap();
    registry.queue_record(2, record(NodeId::from_index(0))).unwrap();

    assert_eq!(registry.total_records, 1);
    assert!(registry.observers.contains_key(&1));
    assert!(!registry.observers.contains_key(&2));
    assert_eq!(registry.pending, vec![1]);
  }
}
