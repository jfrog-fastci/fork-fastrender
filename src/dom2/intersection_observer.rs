use crate::geometry::Rect;

use super::{DomError, Document, NodeId, NodeKind};
use std::collections::HashMap;

pub type IntersectionObserverId = u64;

#[derive(Debug, Clone)]
pub struct IntersectionObserverInit {
  pub root: Option<NodeId>,
  pub root_margin: String,
  pub thresholds: Vec<f64>,
}

impl Default for IntersectionObserverInit {
  fn default() -> Self {
    Self {
      root: None,
      root_margin: "0px".to_string(),
      thresholds: vec![0.0],
    }
  }
}

#[derive(Debug, Clone)]
pub struct IntersectionObserverEntry {
  pub time: f64,
  pub target: NodeId,
  pub root_bounds: Option<Rect>,
  pub bounding_client_rect: Rect,
  pub intersection_rect: Rect,
  pub is_intersecting: bool,
  pub intersection_ratio: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntersectionObserverLimits {
  pub max_observers: usize,
  pub max_records_per_observer: usize,
  pub max_total_records: usize,
}

impl Default for IntersectionObserverLimits {
  fn default() -> Self {
    Self {
      max_observers: 10_000,
      max_records_per_observer: 10_000,
      max_total_records: 100_000,
    }
  }
}

/// Internal `IntersectionObserver` registry.
///
/// The registry maintains node-indexed registration storage sized to `Document.nodes`, plus a per-observer
/// queue of pending entries (for `takeRecords()` + callback delivery).
#[derive(Debug, Clone)]
pub(crate) struct IntersectionObserverRegistry {
  limits: IntersectionObserverLimits,
  observers: HashMap<IntersectionObserverId, ObserverState>,
  /// Per-node registered observers (indexed by `NodeId.index()`).
  registrations: Vec<Vec<IntersectionObserverId>>,
  pending: Vec<IntersectionObserverId>,
  total_records: usize,
}

#[derive(Debug, Clone)]
struct ObserverState {
  init: IntersectionObserverInit,
  records: Vec<IntersectionObserverEntry>,
  observed_targets: Vec<NodeId>,
  in_pending: bool,
}

impl IntersectionObserverRegistry {
  pub(crate) fn new(nodes_len: usize) -> Self {
    Self {
      limits: IntersectionObserverLimits::default(),
      observers: HashMap::new(),
      registrations: vec![Vec::new(); nodes_len],
      pending: Vec::new(),
      total_records: 0,
    }
  }

  /// Notify the registry that `Document` has appended a new node to `Document.nodes`.
  pub(crate) fn on_node_added(&mut self) {
    self.registrations.push(Vec::new());
  }

  pub(crate) fn limits(&self) -> IntersectionObserverLimits {
    self.limits
  }

  pub(crate) fn set_limits(&mut self, limits: IntersectionObserverLimits) {
    self.limits = limits;
  }

  fn state_for_observer_mut(
    &mut self,
    observer: IntersectionObserverId,
    init: IntersectionObserverInit,
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
          init,
          records: Vec::new(),
          observed_targets: Vec::new(),
          in_pending: false,
        }),
    )
  }

  fn queue_entry(&mut self, observer: IntersectionObserverId, record: IntersectionObserverEntry) {
    let Some(state) = self.observers.get_mut(&observer) else {
      // The observer is not registered (for example, disconnected or GC'd); ignore stale deliveries.
      return;
    };
    let limits = self.limits;
    if self.total_records >= limits.max_total_records {
      return;
    }
    if state.records.len() >= limits.max_records_per_observer {
      return;
    }
    state.records.push(record);
    self.total_records = self.total_records.saturating_add(1);

    if !state.in_pending {
      state.in_pending = true;
      self.pending.push(observer);
    }
  }
}

fn is_intersection_observer_root_kind(kind: &NodeKind) -> bool {
  matches!(
    kind,
    NodeKind::Document { .. } | NodeKind::Element { .. } | NodeKind::Slot { .. }
  )
}

fn is_intersection_observer_target_kind(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

impl Document {
  pub fn intersection_observer_limits(&self) -> IntersectionObserverLimits {
    self.intersection_observers.limits()
  }

  pub fn set_intersection_observer_limits(&mut self, limits: IntersectionObserverLimits) {
    self.intersection_observers.set_limits(limits);
  }

  pub fn intersection_observer_observe(
    &mut self,
    observer: IntersectionObserverId,
    target: NodeId,
    init: IntersectionObserverInit,
  ) -> Result<(), DomError> {
    let target_node = self.node_checked(target)?;
    if !is_intersection_observer_target_kind(&target_node.kind) {
      return Err(DomError::InvalidNodeTypeError);
    }

    if let Some(root) = init.root {
      let root_node = self.node_checked(root)?;
      if !is_intersection_observer_root_kind(&root_node.kind) {
        return Err(DomError::InvalidNodeTypeError);
      }
    }

    // Remove any existing registration for (target, observer).
    if let Some(existing) = self.intersection_observers.registrations.get_mut(target.index()) {
      existing.retain(|&id| id != observer);
      existing.push(observer);
    }

    let state = self
      .intersection_observers
      .state_for_observer_mut(observer, init.clone())?;
    state.init = init;
    if !state.observed_targets.contains(&target) {
      state.observed_targets.push(target);
    }

    Ok(())
  }

  pub fn intersection_observer_unobserve(&mut self, observer: IntersectionObserverId, target: NodeId) {
    let Some(list) = self.intersection_observers.registrations.get_mut(target.index()) else {
      return;
    };
    list.retain(|&id| id != observer);

    let Some(state) = self.intersection_observers.observers.get_mut(&observer) else {
      return;
    };
    state.observed_targets.retain(|&id| id != target);
  }

  pub fn intersection_observer_disconnect(&mut self, observer: IntersectionObserverId) {
    let Some(state) = self.intersection_observers.observers.remove(&observer) else {
      return;
    };

    self.intersection_observers.total_records = self
      .intersection_observers
      .total_records
      .saturating_sub(state.records.len());

    if state.in_pending {
      self
        .intersection_observers
        .pending
        .retain(|&id| id != observer);
    }

    for target in state.observed_targets {
      if let Some(list) = self.intersection_observers.registrations.get_mut(target.index()) {
        list.retain(|&id| id != observer);
      }
    }
  }

  pub fn intersection_observer_take_records(
    &mut self,
    observer: IntersectionObserverId,
  ) -> Vec<IntersectionObserverEntry> {
    let Some(state) = self.intersection_observers.observers.get_mut(&observer) else {
      return Vec::new();
    };
    self.intersection_observers.total_records = self
      .intersection_observers
      .total_records
      .saturating_sub(state.records.len());
    std::mem::take(&mut state.records)
  }

  pub fn intersection_observer_take_deliveries(
    &mut self,
  ) -> Vec<(IntersectionObserverId, Vec<IntersectionObserverEntry>)> {
    let pending = std::mem::take(&mut self.intersection_observers.pending);
    let mut out: Vec<(IntersectionObserverId, Vec<IntersectionObserverEntry>)> = Vec::new();
    for observer in pending {
      let Some(state) = self.intersection_observers.observers.get_mut(&observer) else {
        continue;
      };
      state.in_pending = false;
      if state.records.is_empty() {
        continue;
      }
      self.intersection_observers.total_records = self
        .intersection_observers
        .total_records
        .saturating_sub(state.records.len());
      out.push((observer, std::mem::take(&mut state.records)));
    }
    out
  }

  /// Queue an IntersectionObserver entry for later delivery to JS.
  ///
  /// This is intended to be called by embedding layers that compute intersection observations (for
  /// example, during layout).
  pub fn intersection_observer_queue_entry(
    &mut self,
    observer: IntersectionObserverId,
    entry: IntersectionObserverEntry,
  ) {
    self.intersection_observers.queue_entry(observer, entry);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;

  #[test]
  fn on_node_added_keeps_node_indexed_storage_sized() {
    let mut registry = IntersectionObserverRegistry::new(0);
    assert_eq!(registry.registrations.len(), 0);
    registry.on_node_added();
    registry.on_node_added();
    registry.on_node_added();
    assert_eq!(registry.registrations.len(), 3);
    assert!(registry.registrations.iter().all(|v| v.is_empty()));
  }

  #[test]
  fn observe_after_node_is_created_does_not_panic() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let div = doc.create_element("div", "");

    assert_eq!(
      doc.intersection_observers.registrations.len(),
      doc.nodes_len(),
      "intersection observer registry must stay in sync with Document.nodes"
    );

    doc
      .intersection_observer_observe(1, div, IntersectionObserverInit::default())
      .expect("observe should succeed");
    assert_eq!(doc.intersection_observers.registrations[div.index()], vec![1]);
  }
}
