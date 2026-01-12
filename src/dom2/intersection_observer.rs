use super::NodeId;

pub type IntersectionObserverId = u64;

/// Internal `IntersectionObserver` registry.
///
/// This is currently a minimal implementation that focuses on maintaining per-node storage sized to
/// `Document.nodes`. The per-node vectors are indexed by `NodeId.index()`.
#[derive(Debug, Clone)]
pub(crate) struct IntersectionObserverRegistry {
  /// Per-node registrations (indexed by `NodeId.index()`).
  registrations: Vec<Vec<IntersectionObserverId>>,
}

impl IntersectionObserverRegistry {
  pub(crate) fn new(nodes_len: usize) -> Self {
    Self {
      registrations: vec![Vec::new(); nodes_len],
    }
  }

  /// Notify the registry that `Document` has appended a new node to `Document.nodes`.
  pub(crate) fn on_node_added(&mut self) {
    self.registrations.push(Vec::new());
  }

  pub(crate) fn observe(&mut self, observer: IntersectionObserverId, target: NodeId) {
    if let Some(list) = self.registrations.get_mut(target.index()) {
      list.push(observer);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::Document;
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

    doc.intersection_observers.observe(1, div);
    assert_eq!(doc.intersection_observers.registrations[div.index()], vec![1]);
  }
}
