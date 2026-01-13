//! Minimal host-side HTMLMediaElement state registry.
//!
//! FastRender's DOM node wrappers are GC-managed JS objects, while most per-element backing state
//! lives on the host side (Rust). When pages create and discard many media elements, we must ensure
//! any host-side registry entries are eventually removed; otherwise the per-realm registry can grow
//! without bound.
//!
//! The registry is keyed by [`DomNodeKey`] (document id + node id) to avoid collisions across
//! multiple documents in the same JS realm.

use crate::dom2;
use crate::js::dom_platform::{DocumentId, DomNodeKey, DomPlatform};
use std::collections::HashMap;
use vm_js::Heap;

/// Minimal backing state for an `HTMLMediaElement` (`<audio>` / `<video>`).
///
/// This intentionally does **not** store any GC-managed `Value`/`GcObject` handles; the state should
/// remain cheap to keep on the host side.
#[derive(Debug, Clone)]
pub(crate) struct MediaElementState {
  pub paused: bool,
  pub current_time: f64,
}

impl Default for MediaElementState {
  fn default() -> Self {
    Self {
      paused: true,
      current_time: 0.0,
    }
  }
}

#[derive(Debug, Default)]
pub(crate) struct MediaElementStateRegistry {
  last_gc_runs: u64,
  states: HashMap<DomNodeKey, MediaElementState>,
}

impl MediaElementStateRegistry {
  pub(crate) fn len(&self) -> usize {
    self.states.len()
  }

  #[cfg(test)]
  pub(crate) fn is_empty(&self) -> bool {
    self.states.is_empty()
  }

  pub(crate) fn get_or_create(&mut self, key: DomNodeKey) -> &mut MediaElementState {
    self.states.entry(key).or_default()
  }

  /// Opportunistically sweep unreachable media element state entries.
  ///
  /// Sweeping is triggered by `vm-js` GC runs: whenever `heap.gc_runs()` changes, we remove entries
  /// that are no longer needed.
  ///
  /// Removal conditions (best effort):
  /// - The corresponding DOM wrapper is no longer reachable (collected), as indicated by
  ///   [`DomPlatform`] wrapper caches.
  /// - The node id is no longer valid in the corresponding `dom2::Document`.
  pub(crate) fn sweep_if_needed(
    &mut self,
    heap: &Heap,
    dom_platform: Option<&mut DomPlatform>,
    host_dom: Option<&dom2::Document>,
    owned_dom2_documents: Option<&HashMap<DocumentId, Box<dom2::Document>>>,
  ) -> bool {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return false;
    }
    self.last_gc_runs = gc_runs;

    if self.states.is_empty() {
      return true;
    }

    let mut dom_platform = dom_platform;
    self.states.retain(|key, _state| {
      // 1) Wrapper reachability (primary bound): if the wrapper has been collected, drop host-side
      // state.
      if let Some(platform) = dom_platform.as_deref_mut() {
        if platform
          .get_existing_wrapper_for_document_id(heap, key.document_id, key.node_id)
          .is_none()
        {
          return false;
        }
      }

      // 2) Node validity: if the node id is out-of-bounds for the backing `dom2::Document`, drop the
      // entry to avoid accumulating stale state (e.g. after document replacement/remapping).
      let dom = owned_dom2_documents
        .and_then(|docs| docs.get(&key.document_id).map(|dom| dom.as_ref()))
        .or(host_dom);
      if let Some(dom) = dom {
        if dom.node_id_from_index(key.node_id.index()).is_err() {
          return false;
        }
      }

      true
    });

    true
  }
}
