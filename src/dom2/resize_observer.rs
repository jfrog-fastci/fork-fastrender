use crate::geometry::Rect;

use super::{DomError, Document, NodeId, NodeKind};
use std::collections::HashMap;

pub type ResizeObserverId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeObserverBoxOptions {
  ContentBox,
  BorderBox,
  DevicePixelContentBox,
}

impl Default for ResizeObserverBoxOptions {
  fn default() -> Self {
    Self::ContentBox
  }
}

#[derive(Debug, Clone)]
pub struct ResizeObserverSize {
  pub inline_size: f64,
  pub block_size: f64,
}

#[derive(Debug, Clone)]
pub struct ResizeObserverEntry {
  pub target: NodeId,
  pub content_rect: Rect,
  pub border_box_size: ResizeObserverSize,
  pub content_box_size: ResizeObserverSize,
  pub device_pixel_content_box_size: ResizeObserverSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeObserverLimits {
  pub max_observers: usize,
  pub max_records_per_observer: usize,
  pub max_total_records: usize,
}

impl Default for ResizeObserverLimits {
  fn default() -> Self {
    Self {
      max_observers: 10_000,
      max_records_per_observer: 10_000,
      max_total_records: 100_000,
    }
  }
}

/// Internal `ResizeObserver` registry.
///
/// The registry maintains node-indexed registration storage sized to `Document.nodes`, plus a per-observer
/// queue of pending entries (for `takeRecords()` + callback delivery).
#[derive(Debug, Clone)]
pub(crate) struct ResizeObserverRegistry {
  limits: ResizeObserverLimits,
  observers: HashMap<ResizeObserverId, ObserverState>,
  /// Per-node registered observers (indexed by `NodeId.index()`).
  registrations: Vec<Vec<Registration>>,
  pending: Vec<ResizeObserverId>,
  total_records: usize,
}

#[derive(Debug, Clone)]
struct ObserverState {
  records: Vec<ResizeObserverEntry>,
  observed_targets: Vec<NodeId>,
  in_pending: bool,
}

#[derive(Debug, Clone, Copy)]
struct Registration {
  observer: ResizeObserverId,
  box_: ResizeObserverBoxOptions,
}

impl ResizeObserverRegistry {
  pub(crate) fn new(nodes_len: usize) -> Self {
    Self {
      limits: ResizeObserverLimits::default(),
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

  pub(crate) fn limits(&self) -> ResizeObserverLimits {
    self.limits
  }

  pub(crate) fn set_limits(&mut self, limits: ResizeObserverLimits) {
    self.limits = limits;
  }

  fn state_for_observer_mut(&mut self, observer: ResizeObserverId) -> Result<&mut ObserverState, DomError> {
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

  fn queue_entry(&mut self, observer: ResizeObserverId, record: ResizeObserverEntry) {
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

fn is_resize_observer_target_kind(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

impl Document {
  pub fn resize_observer_limits(&self) -> ResizeObserverLimits {
    self.resize_observers.limits()
  }

  pub fn set_resize_observer_limits(&mut self, limits: ResizeObserverLimits) {
    self.resize_observers.set_limits(limits);
  }

  pub fn resize_observer_observe(
    &mut self,
    observer: ResizeObserverId,
    target: NodeId,
    box_: ResizeObserverBoxOptions,
  ) -> Result<(), DomError> {
    let target_node = self.node_checked(target)?;
    if !is_resize_observer_target_kind(&target_node.kind) {
      return Err(DomError::InvalidNodeTypeError);
    }

    // Remove any existing registration for (target, observer).
    if let Some(existing) = self.resize_observers.registrations.get_mut(target.index()) {
      existing.retain(|reg| reg.observer != observer);
      existing.push(Registration { observer, box_ });
    }

    let state = self.resize_observers.state_for_observer_mut(observer)?;
    if !state.observed_targets.contains(&target) {
      state.observed_targets.push(target);
    }

    Ok(())
  }

  pub fn resize_observer_unobserve(&mut self, observer: ResizeObserverId, target: NodeId) {
    let Some(list) = self.resize_observers.registrations.get_mut(target.index()) else {
      return;
    };
    list.retain(|reg| reg.observer != observer);

    let Some(state) = self.resize_observers.observers.get_mut(&observer) else {
      return;
    };
    state.observed_targets.retain(|&id| id != target);
  }

  pub fn resize_observer_disconnect(&mut self, observer: ResizeObserverId) {
    let Some(state) = self.resize_observers.observers.remove(&observer) else {
      return;
    };

    self.resize_observers.total_records = self
      .resize_observers
      .total_records
      .saturating_sub(state.records.len());

    if state.in_pending {
      self.resize_observers.pending.retain(|&id| id != observer);
    }

    for target in state.observed_targets {
      if let Some(list) = self.resize_observers.registrations.get_mut(target.index()) {
        list.retain(|reg| reg.observer != observer);
      }
    }
  }

  pub fn resize_observer_take_records(&mut self, observer: ResizeObserverId) -> Vec<ResizeObserverEntry> {
    let Some(state) = self.resize_observers.observers.get_mut(&observer) else {
      return Vec::new();
    };
    self.resize_observers.total_records = self
      .resize_observers
      .total_records
      .saturating_sub(state.records.len());
    std::mem::take(&mut state.records)
  }

  pub fn resize_observer_take_deliveries(&mut self) -> Vec<(ResizeObserverId, Vec<ResizeObserverEntry>)> {
    let pending = std::mem::take(&mut self.resize_observers.pending);
    let mut out: Vec<(ResizeObserverId, Vec<ResizeObserverEntry>)> = Vec::new();
    for observer in pending {
      let Some(state) = self.resize_observers.observers.get_mut(&observer) else {
        continue;
      };
      state.in_pending = false;
      if state.records.is_empty() {
        continue;
      }
      self.resize_observers.total_records = self
        .resize_observers
        .total_records
        .saturating_sub(state.records.len());
      out.push((observer, std::mem::take(&mut state.records)));
    }
    out
  }

  /// Queue a ResizeObserver entry for later delivery to JS.
  ///
  /// This is intended to be called by embedding layers that compute element sizes (for example,
  /// during layout).
  pub fn resize_observer_queue_entry(&mut self, observer: ResizeObserverId, entry: ResizeObserverEntry) {
    self.resize_observers.queue_entry(observer, entry);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;

  #[test]
  fn on_node_added_keeps_node_indexed_storage_sized() {
    let mut registry = ResizeObserverRegistry::new(0);
    assert_eq!(registry.registrations.len(), 0);
    registry.on_node_added();
    registry.on_node_added();
    assert_eq!(registry.registrations.len(), 2);
    assert!(registry.registrations.iter().all(|v| v.is_empty()));
  }

  #[test]
  fn observe_after_node_is_created_does_not_panic() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let div = doc.create_element("div", "");

    assert_eq!(
      doc.resize_observers.registrations.len(),
      doc.nodes_len(),
      "resize observer registry must stay in sync with Document.nodes"
    );

    doc
      .resize_observer_observe(1, div, ResizeObserverBoxOptions::ContentBox)
      .expect("observe should succeed");
    assert_eq!(doc.resize_observers.registrations[div.index()].len(), 1);
    assert_eq!(doc.resize_observers.registrations[div.index()][0].observer, 1);
  }
}
