use super::{Document, NodeId};
use std::collections::HashMap;
use vm_js::{GcObject, Heap, WeakGcObject};

/// Stable monotonic identifier for a live `Range` (DOM Standard) registered against a `dom2::Document`.
///
/// Host-side only: the ID is used by the embedding / bindings layer and is not exposed to JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LiveRangeId(u64);

/// Stable monotonic identifier for a live `NodeIterator` (DOM Standard) registered against a
/// `dom2::Document`.
///
/// Host-side only: the ID is used by the embedding / bindings layer and is not exposed to JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NodeIteratorId(u64);

/// Hook surface for "live" DOM traversal state (e.g. `Range`, `NodeIterator`) that must stay in
/// sync with `dom2` mutations.
///
/// Call order matters: hook methods are invoked *before* the corresponding structural/text
/// mutation is applied to the backing `Node::children` list or text storage.
pub(crate) trait LiveMutationHook {
  /// A pending insertion of `count` children into `parent` at `index`.
  ///
  /// This is called after inputs have been validated and the final insertion index has been
  /// determined, but before mutating `parent`'s `children` list.
  fn pre_insert(&mut self, parent: NodeId, index: usize, count: usize);

  /// A pending removal of `node` from `old_parent` at `old_index`.
  ///
  /// This is called before mutating `old_parent`'s `children` list.
  fn pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize);

  /// A pending character-data replacement in `node`.
  ///
  /// Lengths are measured in UTF-16 code units, matching DOM `CharacterData`/`Range` offset units.
  fn replace_data(&mut self, node: NodeId, offset: usize, removed_len: usize, inserted_len: usize);
}

/// Shared live-mutation infrastructure owned by `dom2::Document`.
///
/// Responsibilities:
/// - Provide a registry for live `Range` and `NodeIterator` platform objects without keeping JS
///   objects alive (store [`WeakGcObject`] handles only).
/// - Provide mutation hook entry points (`pre_insert`, `pre_remove`, `replace_data`) that will be
///   used by future live Range/NodeIterator update algorithms.
/// - Optional host-side hook injection for tests.
pub(crate) struct LiveMutation {
  hook: Option<Box<dyn LiveMutationHook>>,
  next_live_range_id: u64,
  live_ranges: HashMap<LiveRangeId, WeakGcObject>,
  next_node_iterator_id: u64,
  node_iterators: HashMap<NodeIteratorId, WeakGcObject>,
  last_gc_runs: u64,
}

impl Default for LiveMutation {
  fn default() -> Self {
    Self {
      hook: None,
      next_live_range_id: 1,
      live_ranges: HashMap::new(),
      next_node_iterator_id: 1,
      node_iterators: HashMap::new(),
      last_gc_runs: 0,
    }
  }
}

impl LiveMutation {
  pub(crate) fn set_hook(&mut self, hook: Option<Box<dyn LiveMutationHook>>) {
    self.hook = hook;
  }

  pub(crate) fn register_live_range(&mut self, heap: &Heap, wrapper: GcObject) -> LiveRangeId {
    self.sweep_dead_if_needed(heap);
    let id = LiveRangeId(self.next_live_range_id);
    self.next_live_range_id = self.next_live_range_id.wrapping_add(1);
    self.live_ranges.insert(id, WeakGcObject::from(wrapper));
    id
  }

  pub(crate) fn register_node_iterator(&mut self, heap: &Heap, wrapper: GcObject) -> NodeIteratorId {
    self.sweep_dead_if_needed(heap);
    let id = NodeIteratorId(self.next_node_iterator_id);
    self.next_node_iterator_id = self.next_node_iterator_id.wrapping_add(1);
    self.node_iterators.insert(id, WeakGcObject::from(wrapper));
    id
  }

  pub(crate) fn sweep_dead_if_needed(&mut self, heap: &Heap) {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return;
    }
    self.last_gc_runs = gc_runs;

    self
      .live_ranges
      .retain(|_, weak| weak.upgrade(heap).is_some());
    self
      .node_iterators
      .retain(|_, weak| weak.upgrade(heap).is_some());
  }

  #[inline]
  pub(crate) fn pre_insert(&mut self, parent: NodeId, index: usize, count: usize) {
    if count == 0 {
      return;
    }
    if let Some(hook) = self.hook.as_mut() {
      hook.pre_insert(parent, index, count);
    }

    // Live Range / NodeIterator update algorithms are implemented in follow-up tasks.
  }

  #[inline]
  pub(crate) fn pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize) {
    if let Some(hook) = self.hook.as_mut() {
      hook.pre_remove(node, old_parent, old_index);
    }

    // Live Range / NodeIterator update algorithms are implemented in follow-up tasks.
  }

  #[inline]
  pub(crate) fn replace_data(
    &mut self,
    node: NodeId,
    offset: usize,
    removed_len: usize,
    inserted_len: usize,
  ) {
    if let Some(hook) = self.hook.as_mut() {
      hook.replace_data(node, offset, removed_len, inserted_len);
    }

    // Live Range / NodeIterator update algorithms are implemented in follow-up tasks.
  }

  #[cfg(test)]
  pub(crate) fn live_range_len(&self) -> usize {
    self.live_ranges.len()
  }
}

impl Document {
  pub(crate) fn live_mutation_pre_insert(&mut self, parent: NodeId, index: usize, count: usize) {
    self.live_mutation.pre_insert(parent, index, count);
  }

  pub(crate) fn live_mutation_pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize) {
    self.live_mutation.pre_remove(node, old_parent, old_index);
  }

  pub(crate) fn live_mutation_replace_data(
    &mut self,
    node: NodeId,
    offset: usize,
    removed_len: usize,
    inserted_len: usize,
  ) {
    self
      .live_mutation
      .replace_data(node, offset, removed_len, inserted_len);
  }

  pub(crate) fn register_live_range(&mut self, heap: &Heap, wrapper: GcObject) -> LiveRangeId {
    self.live_mutation.register_live_range(heap, wrapper)
  }

  pub(crate) fn register_node_iterator(&mut self, heap: &Heap, wrapper: GcObject) -> NodeIteratorId {
    self.live_mutation.register_node_iterator(heap, wrapper)
  }

  pub(crate) fn sweep_dead_live_traversals_if_needed(&mut self, heap: &Heap) {
    self.live_mutation.sweep_dead_if_needed(heap);
  }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveMutationEvent {
  PreInsert {
    parent: NodeId,
    index: usize,
    count: usize,
  },
  PreRemove {
    node: NodeId,
    old_parent: NodeId,
    old_index: usize,
  },
  ReplaceData {
    node: NodeId,
    offset: usize,
    removed_len: usize,
    inserted_len: usize,
  },
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LiveMutationTestRecorder {
  events: std::rc::Rc<std::cell::RefCell<Vec<LiveMutationEvent>>>,
}

#[cfg(test)]
impl LiveMutationTestRecorder {
  pub(crate) fn take(&self) -> Vec<LiveMutationEvent> {
    std::mem::take(&mut *self.events.borrow_mut())
  }
}

#[cfg(test)]
impl LiveMutationHook for LiveMutationTestRecorder {
  fn pre_insert(&mut self, parent: NodeId, index: usize, count: usize) {
    self
      .events
      .borrow_mut()
      .push(LiveMutationEvent::PreInsert { parent, index, count });
  }

  fn pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize) {
    self
      .events
      .borrow_mut()
      .push(LiveMutationEvent::PreRemove {
        node,
        old_parent,
        old_index,
      });
  }

  fn replace_data(&mut self, node: NodeId, offset: usize, removed_len: usize, inserted_len: usize) {
    self
      .events
      .borrow_mut()
      .push(LiveMutationEvent::ReplaceData {
        node,
        offset,
        removed_len,
        inserted_len,
      });
  }
}
