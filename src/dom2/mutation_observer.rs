use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, NodeId, NodeKind};
use std::collections::{HashMap, HashSet};

pub type MutationObserverId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RegisteredObserver {
  pub observer: MutationObserverId,
  pub options: MutationObserverInit,
  /// When present, this entry is a transient registered observer (WHATWG DOM).
  ///
  /// `transient_source` identifies the node whose registered observer entry produced this transient
  /// entry (i.e. the observed ancestor at removal time).
  pub transient_source: Option<NodeId>,
}

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

#[derive(Debug)]
pub struct MutationObserverAgent {
  limits: MutationObserverLimits,
  observers: HashMap<MutationObserverId, ObserverState>,
  pending: Vec<MutationObserverId>,
  microtask_queued: bool,
  microtask_needs_queueing: bool,
  total_records: usize,
}

#[derive(Debug, Clone)]
struct ObserverState {
  records: Vec<MutationRecord>,
  /// The spec's "node list" for the observer.
  ///
  /// This contains all observed targets (and will later also contain transient-registration nodes).
  node_list: Vec<NodeId>,
  in_pending: bool,
}

impl MutationObserverAgent {
  pub fn new() -> Self {
    Self {
      limits: MutationObserverLimits::default(),
      observers: HashMap::new(),
      pending: Vec::new(),
      microtask_queued: false,
      microtask_needs_queueing: false,
      total_records: 0,
    }
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
  /// - each observer's node list
  /// - queued mutation records
  ///
  /// This helper updates those references so subsequent deliveries return the correct JS nodes.
  ///
  /// Entries absent from `mapping` are left unchanged. Node lists are deduplicated after remapping
  /// (preserving order).
  pub(crate) fn remap_node_ids(&mut self, mapping: &HashMap<NodeId, NodeId>) {
    for state in self.observers.values_mut() {
      if !state.node_list.is_empty() {
        let mut deduped: Vec<NodeId> = Vec::with_capacity(state.node_list.len());
        let mut seen: HashSet<NodeId> = HashSet::with_capacity(state.node_list.len());
        for &id in &state.node_list {
          let remapped = mapping.get(&id).copied().unwrap_or(id);
          if seen.insert(remapped) {
            deduped.push(remapped);
          }
        }
        state.node_list = deduped;
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
          node_list: Vec::new(),
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
          node_list: Vec::new(),
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
  /// Move mutation observer registrations from `old` to `new`.
  ///
  /// This is used by DOM operations that are implemented as clone+mapping (rather than in-place
  /// moves) but must preserve JS-visible MutationObserver behavior. Callers should also remap the
  /// observer agent's internal `NodeId` references via [`MutationObserverAgent::remap_node_ids`].
  pub(crate) fn mutation_observer_move_registrations(&mut self, old: NodeId, new: NodeId) {
    if old == new {
      return;
    }
    let old_idx = old.index();
    let new_idx = new.index();
    if old_idx >= self.nodes.len() || new_idx >= self.nodes.len() {
      return;
    }

    let moved = std::mem::take(&mut self.nodes[old_idx].registered_observers);
    if moved.is_empty() {
      return;
    }
    let new_list = &mut self.nodes[new_idx].registered_observers;
    for reg in moved {
      new_list.retain(|r| r.observer != reg.observer);
      new_list.push(reg);
    }
  }

  pub(crate) fn mutation_observer_remap_node_ids(&mut self, mapping: &HashMap<NodeId, NodeId>) {
    self.mutation_observer_agent.borrow_mut().remap_node_ids(mapping);
  }

  pub fn mutation_observer_limits(&self) -> MutationObserverLimits {
    self.mutation_observer_agent.borrow().limits()
  }

  pub fn set_mutation_observer_limits(&mut self, limits: MutationObserverLimits) {
    self.mutation_observer_agent.borrow_mut().set_limits(limits);
  }

  pub fn take_mutation_observer_microtask_needed(&mut self) -> bool {
    let mut agent = self.mutation_observer_agent.borrow_mut();
    std::mem::take(&mut agent.microtask_needs_queueing)
  }

  /// Remap internal mutation-observer [`NodeId`] references after a DOM operation replaces node IDs
  /// but wants to preserve wrapper identity.
  ///
  /// This updates:
  /// - each observer's node list, and
  /// - all queued mutation records.
  ///
  /// Callers must also move per-node registrations with [`Self::mutation_observer_move_registrations`]
  /// so future mutations on the new node continue to queue records.
  ///
  /// Entries absent from `mapping` are left unchanged.
  pub(crate) fn mutation_observer_remap_node_ids(&mut self, mapping: &HashMap<NodeId, NodeId>) {
    if mapping.is_empty() {
      return;
    }
    self.mutation_observer_agent.borrow_mut().remap_node_ids(mapping);
  }

  /// Move mutation observer registrations stored on `old` to `new`.
  ///
  /// This is intended for clone+mapping style operations (e.g. adoption/import approximations) that
  /// preserve wrapper identity by updating wrapper objects to point at a different `NodeId`.
  ///
  /// If either node is missing, this is a no-op.
  pub(crate) fn mutation_observer_move_registrations(&mut self, old: NodeId, new: NodeId) {
    if old == new {
      return;
    }
    let old_idx = old.index();
    let new_idx = new.index();
    if old_idx >= self.nodes.len() || new_idx >= self.nodes.len() {
      return;
    }
    if old_idx == new_idx {
      return;
    }

    let moved = std::mem::take(&mut self.nodes[old_idx].registered_observers);
    if moved.is_empty() {
      return;
    }
    let new_list = &mut self.nodes[new_idx].registered_observers;
    for reg in moved {
      new_list.retain(|r| {
        !(r.observer == reg.observer && r.transient_source == reg.transient_source)
      });
      new_list.push(reg);
    }
  }

  pub fn mutation_observer_observe(
    &mut self,
    observer: MutationObserverId,
    target: NodeId,
    options: MutationObserverInit,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    // If we already have a non-transient registration for (target, observer), remove transient
    // registrations sourced from it from every node in the observer's node list.
    //
    // DOM: observe() updates an existing registered observer's options in-place, but first removes
    // transient registered observers whose source is that registered observer.
    let had_non_transient_registration = self.nodes[target.index()]
      .registered_observers
      .iter()
      .any(|reg| reg.observer == observer && reg.transient_source.is_none());
    if had_non_transient_registration {
      let node_list = self
        .mutation_observer_agent
        .borrow()
        .observers
        .get(&observer)
        .map(|state| state.node_list.clone())
        .unwrap_or_default();
      for node_id in node_list {
        let Some(node) = self.nodes.get_mut(node_id.index()) else {
          continue;
        };
        node
          .registered_observers
          .retain(|reg| !(reg.observer == observer && reg.transient_source == Some(target)));
      }
    }

    // Replace any existing registrations for (target, observer) with a new non-transient one.
    {
      let node = self.node_checked_mut(target)?;
      node
        .registered_observers
        .retain(|reg| reg.observer != observer);
      node.registered_observers.push(RegisteredObserver {
        observer,
        options: options.clone(),
        transient_source: None,
      });
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
    let state = agent.state_for_observer_mut(observer)?;
    if !state.node_list.contains(&target) {
      state.node_list.push(target);
    }

    Ok(())
  }

  pub fn mutation_observer_disconnect(&mut self, observer: MutationObserverId) {
    let state = {
      let mut agent = self.mutation_observer_agent.borrow_mut();
      let Some(state) = agent.observers.remove(&observer) else {
        return;
      };

      agent.total_records = agent.total_records.saturating_sub(state.records.len());

      if state.in_pending {
        agent.pending.retain(|&id| id != observer);
      }
      state
    };

    for target in state.node_list {
      let Some(node) = self.nodes.get_mut(target.index()) else {
        continue;
      };
      node
        .registered_observers
        .retain(|reg| reg.observer != observer);
    }
  }

  pub fn mutation_observer_take_records(&mut self, observer: MutationObserverId) -> Vec<MutationRecord> {
    let mut agent = self.mutation_observer_agent.borrow_mut();
    let (records, record_count) = {
      let Some(state) = agent.observers.get_mut(&observer) else {
        return Vec::new();
      };
      let records = std::mem::take(&mut state.records);
      let record_count = records.len();
      (records, record_count)
    };
    agent.total_records = agent.total_records.saturating_sub(record_count);
    records
  }

  pub fn mutation_observer_take_deliveries(&mut self) -> Vec<(MutationObserverId, Vec<MutationRecord>)> {
    let pending = {
      let mut agent = self.mutation_observer_agent.borrow_mut();
      agent.microtask_queued = false;
      agent.microtask_needs_queueing = false;
      std::mem::take(&mut agent.pending)
    };
    let mut out: Vec<(MutationObserverId, Vec<MutationRecord>)> = Vec::new();
    for observer in pending {
      let (node_list, records) = {
        let mut agent = self.mutation_observer_agent.borrow_mut();
        let (node_list, records, record_count) = {
          let Some(state) = agent.observers.get_mut(&observer) else {
            continue;
          };
          state.in_pending = false;
          let node_list = state.node_list.clone();
          let records = std::mem::take(&mut state.records);
          let record_count = records.len();
          (node_list, records, record_count)
        };
        agent.total_records = agent.total_records.saturating_sub(record_count);
        (node_list, records)
      };

      // DOM: notify mutation observers removes all transient registered observers for `observer`.
      self.mutation_observer_cleanup_transient_registrations(observer, &node_list);

      if !records.is_empty() {
        out.push((observer, records));
      }
    }
    out
  }

  pub(crate) fn mutation_observer_add_transient_observers_on_remove(
    &mut self,
    node: NodeId,
    parent: NodeId,
  ) -> Result<(), DomError> {
    self.node_checked(node)?;
    self.node_checked(parent)?;

    // DOM `remove` step: for each inclusive ancestor of `parent`, for each registered observer with
    // subtree=true, append a transient registered observer to `node`'s registered observer list.
    //
    // Note: We clone registrations first to avoid borrowing `self.nodes` mutably while iterating.
    #[derive(Clone)]
    struct TransientToAdd {
      observer: MutationObserverId,
      options: MutationObserverInit,
      source: NodeId,
    }

    let mut to_add: Vec<TransientToAdd> = Vec::new();
    let mut current = Some(parent);
    while let Some(ancestor) = current {
      let list = &self.nodes[ancestor.index()].registered_observers;
      for reg in list {
        if !reg.options.subtree {
          continue;
        }
        let source = reg.transient_source.unwrap_or(ancestor);
        to_add.push(TransientToAdd {
          observer: reg.observer,
          options: reg.options.clone(),
          source,
        });
      }
      current = self.nodes[ancestor.index()].parent;
    }

    if to_add.is_empty() {
      return Ok(());
    }

    let mut observers_to_track: Vec<MutationObserverId> = Vec::new();
    {
      let list = &mut self.nodes[node.index()].registered_observers;
      for t in to_add {
        let exists = list.iter().any(|reg| {
          reg.observer == t.observer && reg.transient_source == Some(t.source)
        });
        if exists {
          continue;
        }
        observers_to_track.push(t.observer);
        list.push(RegisteredObserver {
          observer: t.observer,
          options: t.options,
          transient_source: Some(t.source),
        });
      }
    }

    // Ensure the observer's node list includes `node` so transient cleanup can find it.
    observers_to_track.sort_unstable();
    observers_to_track.dedup();
    let mut agent = self.mutation_observer_agent.borrow_mut();
    for observer in observers_to_track {
      let state = agent.state_for_observer_mut(observer)?;
      if !state.node_list.contains(&node) {
        state.node_list.push(node);
      }
    }

    Ok(())
  }

  fn mutation_observer_cleanup_transient_registrations(
    &mut self,
    observer: MutationObserverId,
    node_list: &[NodeId],
  ) {
    // Collect nodes that still have any (non-transient) registration for this observer after
    // removing transient ones, so we can keep the observer's node list from growing without bound.
    let mut keep_nodes: Vec<NodeId> = Vec::new();

    for &node_id in node_list {
      let Some(node) = self.nodes.get_mut(node_id.index()) else {
        continue;
      };

      node.registered_observers.retain(|reg| {
        !(reg.observer == observer && reg.transient_source.is_some())
      });

      let still_observing = node
        .registered_observers
        .iter()
        .any(|reg| reg.observer == observer);
      if still_observing {
        keep_nodes.push(node_id);
      }
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
    if let Some(state) = agent.observers.get_mut(&observer) {
      state.node_list = keep_nodes;
    }
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
      let list = &self.nodes[node.index()].registered_observers;
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
      // MutationObserver walks the inclusive ancestors in the DOM tree. Shadow roots form a tree
      // boundary: observers registered outside the shadow tree must not observe mutations inside it.
      //
      // `dom2` stores shadow roots as nodes whose `parent` points at the host, so treat ShadowRoot as
      // having no parent for the purpose of this ancestor walk.
      if matches!(self.nodes[node.index()].kind, NodeKind::ShadowRoot { .. }) {
        break;
      }
      let parent = self.nodes[node.index()].parent;
      // Template contents are represented in `dom2` as descendants of the `<template>` element with
      // `inert_subtree=true` on that template. These nodes are not part of the DOM tree, so do not
      // allow MutationObserver registrations outside the inert subtree to observe them.
      if parent.is_some_and(|parent| self.nodes[parent.index()].inert_subtree) {
        break;
      }
      current = parent;
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
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
      agent.queue_record(observer, record)?;
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
      let list = &self.nodes[node.index()].registered_observers;
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
      if matches!(self.nodes[node.index()].kind, NodeKind::ShadowRoot { .. }) {
        break;
      }
      let parent = self.nodes[node.index()].parent;
      if parent.is_some_and(|parent| self.nodes[parent.index()].inert_subtree) {
        break;
      }
      current = parent;
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
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
      agent.queue_record(observer, record)?;
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
      let list = &self.nodes[node.index()].registered_observers;
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
      if matches!(self.nodes[node.index()].kind, NodeKind::ShadowRoot { .. }) {
        break;
      }
      let parent = self.nodes[node.index()].parent;
      if parent.is_some_and(|parent| self.nodes[parent.index()].inert_subtree) {
        break;
      }
      current = parent;
    }

    if interested.is_empty() {
      return Ok(());
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
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
      agent.queue_record(observer, record)?;
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
    let mut agent = MutationObserverAgent::new();
    agent.queue_record(1, record(NodeId::from_index(0))).unwrap();

    assert!(agent.microtask_queued);
    assert!(agent.microtask_needs_queueing);
    assert_eq!(agent.pending, vec![1]);
    assert_eq!(agent.total_records, 1);

    let state = agent.observers.get(&1).unwrap();
    assert_eq!(state.records.len(), 1);
    assert!(state.in_pending);
  }

  #[test]
  fn queue_record_is_bounded_per_observer() {
    let mut agent = MutationObserverAgent::new();
    agent.set_limits(MutationObserverLimits {
      max_observers: 10,
      max_records_per_observer: 1,
      max_total_records: 10,
    });

    agent.queue_record(1, record(NodeId::from_index(0))).unwrap();
    agent.queue_record(1, record(NodeId::from_index(0))).unwrap();

    assert_eq!(agent.total_records, 1);
    assert_eq!(agent.pending, vec![1]);
    assert_eq!(agent.observers.get(&1).unwrap().records.len(), 1);
  }

  #[test]
  fn queue_record_is_bounded_globally() {
    let mut agent = MutationObserverAgent::new();
    agent.set_limits(MutationObserverLimits {
      max_observers: 10,
      max_records_per_observer: 10,
      max_total_records: 1,
    });

    agent.queue_record(1, record(NodeId::from_index(0))).unwrap();
    agent.queue_record(2, record(NodeId::from_index(0))).unwrap();

    assert_eq!(agent.total_records, 1);
    assert!(agent.observers.contains_key(&1));
    assert!(!agent.observers.contains_key(&2));
    assert_eq!(agent.pending, vec![1]);
  }
}
