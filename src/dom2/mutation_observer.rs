use super::{Document, DomError, Node, NodeId, NodeKind};
use std::collections::{HashMap, HashSet};

pub type MutationObserverId = u64;
type RegistrationId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RegisteredObserver {
  pub id: RegistrationId,
  pub observer: MutationObserverId,
  pub options: MutationObserverInit,
  /// When set, this entry is a "transient registered observer" as defined by the WHATWG DOM
  /// Standard.
  ///
  /// The value points at the `id` of the source registered observer that caused this entry to be
  /// created.
  pub transient_source: Option<RegistrationId>,
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
  next_registration_id: RegistrationId,
}

#[derive(Debug, Clone)]
struct ObserverState {
  records: Vec<MutationRecord>,
  /// The spec's "node list" for the observer.
  ///
  /// This contains all observed targets and nodes that currently carry transient registrations for
  /// this observer.
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
      next_registration_id: 1,
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
  /// - each observer's node list (observed targets + transient registrations)
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

  fn alloc_registration_id(&mut self) -> RegistrationId {
    let id = self.next_registration_id;
    self.next_registration_id = self.next_registration_id.wrapping_add(1);
    id
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

impl Document {
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
    self
      .mutation_observer_agent
      .borrow_mut()
      .remap_node_ids(mapping);
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

    let moved = {
      let Some(old_node) = self.nodes.get_mut(old.index()) else {
        return;
      };
      std::mem::take(&mut old_node.registered_observers)
    };
    if moved.is_empty() {
      return;
    }
    let Some(new_node) = self.nodes.get_mut(new.index()) else {
      // Restore registrations if the target node does not exist.
      if let Some(old_node) = self.nodes.get_mut(old.index()) {
        old_node.registered_observers = moved;
      }
      return;
    };
    // Merge registrations so an observer+transient_source pair appears only once per node.
    for reg in moved {
      if let Some(existing) = new_node
        .registered_observers
        .iter_mut()
        .find(|r| r.observer == reg.observer && r.transient_source == reg.transient_source)
      {
        existing.options = reg.options;
      } else {
        new_node.registered_observers.push(reg);
      }
    }
  }
  pub fn mutation_observer_observe(
    &mut self,
    observer: MutationObserverId,
    target: NodeId,
    options: MutationObserverInit,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    // WHATWG DOM observe(): if an observer is already registered on `target`, update the existing
    // (non-transient) registration and remove transient registered observers sourced from that
    // registration from every node in the observer's node list.
    //
    // Transient registered observers are not treated as existing registrations for observe(), so
    // `observe()` on a node that currently only has transient registrations will create a new
    // non-transient registration that survives the next transient-cleanup pass.
    let existing_registration_id = self.nodes[target.index()]
      .registered_observers
      .iter()
      .find(|reg| reg.observer == observer && reg.transient_source.is_none())
      .map(|reg| reg.id);

    if let Some(existing_registration_id) = existing_registration_id {
      // Spec: if updating an existing registration, remove transient registered observers whose
      // source is the updated registration from all nodes in the observer's node list.
      let nodes_to_cleanup = {
        let mut agent = self.mutation_observer_agent.borrow_mut();
        let state = agent.state_for_observer_mut(observer)?;
        if !state.node_list.contains(&target) {
          state.node_list.push(target);
        }
        state.node_list.clone()
      };

      for node_id in nodes_to_cleanup {
        let Some(node) = self.nodes.get_mut(node_id.index()) else {
          continue;
        };
        // `MutationObserverAgent` can be shared across multiple `dom2::Document` instances.
        //
        // `NodeId` is document-local, so a `node_list` entry may refer to a node that lives in a
        // different document. Avoid mutating unrelated nodes by only touching nodes that actually
        // have registrations for this observer.
        let has_registration = node
          .registered_observers
          .iter()
          .any(|reg| reg.observer == observer);
        if !has_registration {
          continue;
        }
        node
          .registered_observers
          .retain(|reg| reg.transient_source != Some(existing_registration_id));
      }

      {
        let node = self.node_checked_mut(target)?;
        if let Some(reg) = node
          .registered_observers
          .iter_mut()
          .find(|reg| reg.observer == observer && reg.transient_source.is_none())
        {
          reg.options = options;
        }
      }
    } else {
      // Ensure the observer state exists (and we're within observer limits) before mutating per-node
      // registration lists.
      let transient_ids: Vec<RegistrationId> = self.nodes[target.index()]
        .registered_observers
        .iter()
        .filter(|reg| reg.observer == observer && reg.transient_source.is_some())
        .map(|reg| reg.id)
        .collect();

      let id = {
        let mut agent = self.mutation_observer_agent.borrow_mut();
        let state = agent.state_for_observer_mut(observer)?;
        if !state.node_list.contains(&target) {
          state.node_list.push(target);
        }
        agent.alloc_registration_id()
      };

      // If `target` currently has transient registrations for this observer (e.g. inherited from an
      // observed ancestor during a previous `remove()` step), those transient registrations may have
      // created further transient registrations on other nodes (nested transients).
      //
      // Since we're about to create a *new* non-transient registration on `target`, we should remove
      // any nested transients whose source is one of the transient registrations currently on
      // `target`, mirroring observe() update cleanup semantics.
      if !transient_ids.is_empty() {
        let nodes_to_cleanup = {
          let mut agent = self.mutation_observer_agent.borrow_mut();
          agent
            .observers
            .get_mut(&observer)
            .map(|state| state.node_list.clone())
            .unwrap_or_default()
        };
        let transient_ids: HashSet<RegistrationId> = transient_ids.into_iter().collect();
        for node_id in nodes_to_cleanup {
          let Some(node) = self.nodes.get_mut(node_id.index()) else {
            continue;
          };
          let has_registration = node
            .registered_observers
            .iter()
            .any(|reg| reg.observer == observer);
          if !has_registration {
            continue;
          }
          node.registered_observers.retain(|reg| {
            !(reg.observer == observer
              && reg
                .transient_source
                .is_some_and(|src| transient_ids.contains(&src)))
          });
        }
      }

      let node = self.node_checked_mut(target)?;
      // A node can carry transient registrations for `observer` (from an observed ancestor) even if
      // `observer` has never been explicitly registered on this node via `observe()`.
      //
      // These transients must not shadow the newly-created non-transient registration's options.
      node
        .registered_observers
        .retain(|reg| !(reg.observer == observer && reg.transient_source.is_some()));
      node.registered_observers.push(RegisteredObserver {
        id,
        observer,
        options,
        transient_source: None,
      });
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

  pub fn mutation_observer_take_records(
    &mut self,
    observer: MutationObserverId,
  ) -> Vec<MutationRecord> {
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

  pub fn mutation_observer_take_deliveries(
    &mut self,
  ) -> Vec<(MutationObserverId, Vec<MutationRecord>)> {
    let nodes = &mut self.nodes;
    let mut agent = self.mutation_observer_agent.borrow_mut();
    agent.microtask_queued = false;
    agent.microtask_needs_queueing = false;

    let pending = std::mem::take(&mut agent.pending);
    let mut out: Vec<(MutationObserverId, Vec<MutationRecord>)> = Vec::new();
    for observer in pending {
      let Some((node_list, records, record_count)) =
        agent.observers.get_mut(&observer).map(|state| {
          state.in_pending = false;
          let records = std::mem::take(&mut state.records);
          let record_count = records.len();
           let node_list = state.node_list.clone();
           (node_list, records, record_count)
         })
       else {
         continue;
      };
      agent.total_records = agent.total_records.saturating_sub(record_count);

      // DOM: notify mutation observers removes all transient registered observers for `observer`.
      Self::mutation_observer_cleanup_transient_registrations(
        nodes, &mut agent, observer, &node_list,
      );

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
    let mut to_add: Vec<(MutationObserverId, MutationObserverInit, RegistrationId)> = Vec::new();
    let mut current = Some(parent);
    while let Some(ancestor) = current {
      let list = &self.nodes[ancestor.index()].registered_observers;
      for reg in list {
        if reg.options.subtree {
          // Spec: `source` is the `registered` entry from the inclusive ancestor's registered
          // observer list, even if that entry is itself transient (nested transient registrations).
          to_add.push((reg.observer, reg.options.clone(), reg.id));
        }
      }

      // MutationObserver ancestor traversal uses the DOM tree. Shadow roots are tree roots.
      if matches!(
        self.nodes[ancestor.index()].kind,
        NodeKind::ShadowRoot { .. }
      ) {
        break;
      }
      let parent = self.nodes[ancestor.index()].parent;
      // Inert template contents are treated as disconnected; stop before crossing into the inert
      // subtree root (`inert_subtree=true` on the owning template element).
      if parent.is_some_and(|parent| self.nodes[parent.index()].inert_subtree) {
        break;
      }
      current = parent;
    }

    if to_add.is_empty() {
      return Ok(());
    }

    let mut agent = self.mutation_observer_agent.borrow_mut();
    for (observer, options, source_id) in to_add {
      if let Some(list) = self.nodes.get_mut(node.index()).map(|n| &mut n.registered_observers) {
        // Avoid creating duplicate transient registered observers when a node is removed multiple
        // times before the next MutationObserver microtask checkpoint.
        if let Some(existing) = list
          .iter_mut()
          .find(|reg| reg.observer == observer && reg.transient_source == Some(source_id))
        {
          existing.options = options;
        } else {
          let id = agent.alloc_registration_id();
          list.push(RegisteredObserver {
            id,
            observer,
            options,
            transient_source: Some(source_id),
          });
        }
      }

      // The spec's node list is a list of weak references. `dom2` stores stable `NodeId`s for the
      // lifetime of the `Document`, so we maintain a superset of the spec's weak list.
      let state = agent.state_for_observer_mut(observer)?;
      if !state.node_list.contains(&node) {
        state.node_list.push(node);
      }
    }

    Ok(())
  }

  fn mutation_observer_cleanup_transient_registrations(
    nodes: &mut [Node],
    agent: &mut MutationObserverAgent,
    observer: MutationObserverId,
    node_list: &[NodeId],
  ) {
    // Collect nodes that still have any (non-transient) registration for this observer after
    // removing transient ones, so we can keep the observer's node list from growing without bound.
    let mut keep_nodes: Vec<NodeId> = Vec::new();

    for &node_id in node_list {
      let Some(node) = nodes.get_mut(node_id.index()) else {
        // `node_list` entries can refer to nodes in other documents that share the same mutation
        // observer agent. We can't clean those up from this document, but we must keep them so we
        // don't corrupt the observer's node list.
        keep_nodes.push(node_id);
        continue;
      };

      // If this `NodeId` doesn't have any registrations for `observer` in this document, it may be
      // referring to a node from another document. Leave it alone and keep it in the node list.
      //
      // Note: this means stale entries can persist in `node_list`, but that is preferable to
      // incorrectly removing the node list for observers owned by other documents.
      let has_registration = node
        .registered_observers
        .iter()
        .any(|reg| reg.observer == observer);
      if !has_registration {
        keep_nodes.push(node_id);
        continue;
      }

      node
        .registered_observers
        .retain(|reg| !(reg.observer == observer && reg.transient_source.is_some()));

      let still_observing = node
        .registered_observers
        .iter()
        .any(|reg| reg.observer == observer);
      if still_observing {
        keep_nodes.push(node_id);
      }
    }
    if let Some(state) = agent.observers.get_mut(&observer) {
      state.node_list = keep_nodes;
    }
  }

  #[cfg(test)]
  pub(crate) fn mutation_observer_transient_registration_count(&self, node: NodeId) -> usize {
    self
      .nodes
      .get(node.index())
      .map(|node| {
        node
          .registered_observers
          .iter()
          .filter(|reg| reg.transient_source.is_some())
          .count()
      })
      .unwrap_or(0)
  }
  pub(crate) fn queue_mutation_record_attributes(
    &mut self,
    target: NodeId,
    name: &str,
    old_value: Option<String>,
  ) -> Result<(), DomError> {
    self.node_checked(target)?;

    let is_html = match &self.nodes[target.index()].kind {
      NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
        self.is_html_case_insensitive_namespace(namespace)
      }
      _ => false,
    };
    let attr_name = if is_html {
      name.to_ascii_lowercase()
    } else {
      name.to_string()
    };

    // Track which observers are interested in this mutation and whether any matching registration
    // requested recording the old attribute value.
    //
    // Spec: the interested-observers map stores `oldValue` if **any** matching registration has
    // `attributeOldValue=true` (it is not per-registration once the observer is included).
    let mut interested: HashMap<MutationObserverId, bool> = HashMap::new();
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
        let needs_old_value = interested.entry(reg.observer).or_insert(false);
        if reg.options.attribute_old_value {
          *needs_old_value = true;
        }
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
    for (observer, needs_old_value) in interested {
      let record = MutationRecord {
        type_: MutationRecordType::Attributes,
        target,
        added_nodes: Vec::new(),
        removed_nodes: Vec::new(),
        previous_sibling: None,
        next_sibling: None,
        attribute_name: Some(attr_name.clone()),
        old_value: needs_old_value.then(|| old_value.clone()).flatten(),
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

    // Track which observers are interested in this mutation and whether any matching registration
    // requested recording the old character data value.
    let mut interested: HashMap<MutationObserverId, bool> = HashMap::new();
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
        let needs_old_value = interested.entry(reg.observer).or_insert(false);
        if reg.options.character_data_old_value {
          *needs_old_value = true;
        }
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
    for (observer, needs_old_value) in interested {
      let record = MutationRecord {
        type_: MutationRecordType::CharacterData,
        target,
        added_nodes: Vec::new(),
        removed_nodes: Vec::new(),
        previous_sibling: None,
        next_sibling: None,
        attribute_name: None,
        old_value: needs_old_value.then(|| old_value.clone()).flatten(),
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
  use selectors::context::QuirksMode;

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
    agent
      .queue_record(1, record(NodeId::from_index(0)))
      .unwrap();

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

    agent
      .queue_record(1, record(NodeId::from_index(0)))
      .unwrap();
    agent
      .queue_record(1, record(NodeId::from_index(0)))
      .unwrap();

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

    agent
      .queue_record(1, record(NodeId::from_index(0)))
      .unwrap();
    agent
      .queue_record(2, record(NodeId::from_index(0)))
      .unwrap();

    assert_eq!(agent.total_records, 1);
    assert!(agent.observers.contains_key(&1));
    assert!(!agent.observers.contains_key(&2));
    assert_eq!(agent.pending, vec![1]);
  }

  #[test]
  fn observe_update_removes_transient_registrations_from_node_list() {
    let mut doc = Document::new(QuirksMode::NoQuirks);

    let parent = doc.create_element("div", "");
    let child = doc.create_element("div", "");
    let grandchild = doc.create_element("div", "");
    doc.append_child(child, grandchild).unwrap();
    doc.append_child(parent, child).unwrap();

    let observer = 1;
    let initial_options = MutationObserverInit {
      attributes: true,
      subtree: true,
      ..MutationObserverInit::default()
    };
    doc
      .mutation_observer_observe(observer, parent, initial_options)
      .unwrap();

    let parent_registration_id = doc.nodes[parent.index()]
      .registered_observers
      .iter()
      .find(|reg| reg.observer == observer && reg.transient_source.is_none())
      .expect("parent should have non-transient registration after observe()")
      .id;

    // Detach the subtree; removal should install a transient registered observer on the detached
    // root (`child`).
    doc.remove_child(parent, child).unwrap();
    assert_eq!(doc.mutation_observer_transient_registration_count(child), 1);
    assert!(doc.nodes[child.index()]
      .registered_observers
      .iter()
      .any(|reg| {
        reg.observer == observer && reg.transient_source == Some(parent_registration_id)
      }));

    // Mutations inside the detached subtree should still be observed via the transient registration.
    doc.set_attribute(grandchild, "id", "before").unwrap();
    let records = doc.mutation_observer_take_records(observer);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].type_, MutationRecordType::Attributes);
    assert_eq!(records[0].target, grandchild);

    // Updating the observer's registration on `parent` must remove transient registrations sourced
    // from the old registration so detached subtrees stop being observed.
    let updated_options = MutationObserverInit {
      attributes: true,
      subtree: false,
      ..MutationObserverInit::default()
    };
    doc
      .mutation_observer_observe(observer, parent, updated_options)
      .unwrap();

    assert_eq!(doc.mutation_observer_transient_registration_count(child), 0);

    doc.set_attribute(grandchild, "id", "after").unwrap();
    let records = doc.mutation_observer_take_records(observer);
    assert!(records.is_empty());
  }

  #[test]
  fn observe_creates_non_transient_registration_when_only_transient_exists() {
    let mut doc = Document::new(QuirksMode::NoQuirks);

    let parent = doc.create_element("div", "");
    let child = doc.create_element("div", "");
    let grandchild = doc.create_element("div", "");
    doc.append_child(child, grandchild).unwrap();
    doc.append_child(parent, child).unwrap();

    let observer = 1;
    doc
      .mutation_observer_observe(
        observer,
        parent,
        MutationObserverInit {
          attributes: true,
          subtree: true,
          ..MutationObserverInit::default()
        },
      )
      .unwrap();

    let parent_registration_id = doc.nodes[parent.index()]
      .registered_observers
      .iter()
      .find(|reg| reg.observer == observer && reg.transient_source.is_none())
      .unwrap()
      .id;

    // Detach the subtree; `child` should receive a transient registration sourced from `parent`.
    doc.remove_child(parent, child).unwrap();
    assert!(doc.nodes[child.index()]
      .registered_observers
      .iter()
      .any(|reg| {
        reg.observer == observer && reg.transient_source == Some(parent_registration_id)
      }));
    assert!(!doc.nodes[child.index()]
      .registered_observers
      .iter()
      .any(|reg| reg.observer == observer && reg.transient_source.is_none()));

    // `observe(child, ...)` should create a new non-transient registration (transients are not
    // treated as existing registrations for `observe()`).
    doc
      .mutation_observer_observe(
        observer,
        child,
        MutationObserverInit {
          attributes: true,
          subtree: true,
          ..MutationObserverInit::default()
        },
      )
      .unwrap();
    assert!(doc.nodes[child.index()]
      .registered_observers
      .iter()
      .any(|reg| reg.observer == observer && reg.transient_source.is_none()));

    // Simulate the microtask checkpoint that removes transients.
    let _ = doc.mutation_observer_take_deliveries();
    assert_eq!(doc.mutation_observer_transient_registration_count(child), 0);

    // The non-transient registration must remain active after transient cleanup.
    doc.set_attribute(grandchild, "id", "after").unwrap();
    let records = doc.mutation_observer_take_records(observer);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].type_, MutationRecordType::Attributes);
    assert_eq!(records[0].target, grandchild);
  }

  #[test]
  fn observe_on_transient_target_applies_options_immediately() {
    let mut doc = Document::new(QuirksMode::NoQuirks);

    let parent = doc.create_element("div", "");
    let child = doc.create_element("div", "");
    let grandchild = doc.create_element("div", "");
    doc.append_child(child, grandchild).unwrap();
    doc.append_child(parent, child).unwrap();

    let observer = 1;
    doc
      .mutation_observer_observe(
        observer,
        parent,
        MutationObserverInit {
          attributes: true,
          subtree: true,
          ..MutationObserverInit::default()
        },
      )
      .unwrap();

    // Detach the subtree; `child` should receive a transient registration sourced from `parent`.
    doc.remove_child(parent, child).unwrap();
    assert_eq!(doc.mutation_observer_transient_registration_count(child), 1);

    // Observing `child` directly should not leave the transient registration in place, otherwise the
    // transient's (subtree=true) options would shadow the new registration's subtree=false setting.
    doc
      .mutation_observer_observe(
        observer,
        child,
        MutationObserverInit {
          attributes: true,
          subtree: false,
          ..MutationObserverInit::default()
        },
      )
      .unwrap();
    assert_eq!(doc.mutation_observer_transient_registration_count(child), 0);

    // Clear any records queued by the remove above so the final assertion only covers the new
    // mutation below.
    let _ = doc.mutation_observer_take_records(observer);

    doc.set_attribute(grandchild, "id", "after").unwrap();
    let records = doc.mutation_observer_take_records(observer);
    assert!(records.is_empty());
  }

  // Nested transient cleanup is covered by `mutation_observer_observe_update_clears_transients_sourced_from_transients`
  // in `mutation_observer_transient_tests.rs`.
}
