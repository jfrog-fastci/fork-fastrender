use super::{Document, NodeId};
use std::collections::HashMap;
use vm_js::{GcObject, Heap, WeakGcObject};

/// Return the length of a UTF-8 string in UTF-16 code units.
///
/// DOM character-data offsets (e.g. `CharacterData.replaceData` and `Range` boundary points) are
/// defined in terms of UTF-16 code units, matching the semantics of JavaScript strings.
#[inline]
pub(crate) fn utf16_len(s: &str) -> usize {
  s.chars().map(|ch| ch.len_utf16()).sum()
}

/// Stable monotonic identifier for a live `Range` (DOM Standard) registered against a `dom2::Document`.
///
/// Host-side only: the ID is used by the embedding / bindings layer and is not exposed to JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LiveRangeId(u64);

impl LiveRangeId {
  /// Construct a `LiveRangeId` from a raw integer.
  ///
  /// This is primarily used by JS binding layers to store an id in host slots and reconstruct it
  /// later without exposing internal `dom2` state.
  pub fn from_u64(id: u64) -> Self {
    Self(id)
  }

  /// Extract the raw integer value of this id.
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

/// Stable monotonic identifier for a live `NodeIterator` (DOM Standard) registered against a
/// `dom2::Document`.
///
/// Host-side only: the ID is used by the embedding / bindings layer and is not exposed to JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeIteratorId(u64);

impl NodeIteratorId {
  /// Construct a `NodeIteratorId` from a raw integer.
  ///
  /// This is primarily used by JS binding layers to store an id in host slots and reconstruct it
  /// later without exposing internal `dom2` state.
  pub fn from_u64(id: u64) -> Self {
    Self(id)
  }

  /// Extract the raw integer value of this id.
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

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
  /// `offset`, `removed_len`, and `inserted_len` are measured in UTF-16 code units (not bytes and
  /// not Unicode scalar values), matching DOM `CharacterData` / `Range` offset units.
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
  node_iterators: HashMap<NodeIteratorId, WeakGcObject>,
  last_gc_runs: u64,
}

#[derive(Debug, Default)]
pub(crate) struct LiveMutationSweepResult {
  pub(crate) dead_live_ranges: Vec<LiveRangeId>,
  pub(crate) dead_node_iterators: Vec<NodeIteratorId>,
}

/// A live `Range` wrapper/state pair that was transferred to another `dom2::Document`.
///
/// This is used by clone+mapping adoption approximations that preserve JS wrapper identity across
/// documents: ranges whose boundary points remain inside a detached adopted subtree must move to
/// the destination document so subsequent DOM mutations keep their live updates consistent.
pub(crate) struct MovedLiveRange {
  pub(crate) wrapper: GcObject,
  pub(crate) old_id: LiveRangeId,
  pub(crate) new_id: LiveRangeId,
}

impl Default for LiveMutation {
  fn default() -> Self {
    Self {
      hook: None,
      next_live_range_id: 1,
      live_ranges: HashMap::new(),
      node_iterators: HashMap::new(),
      last_gc_runs: 0,
    }
  }
}

impl LiveMutation {
  /// Whether any live traversal state is currently subscribed to mutations.
  ///
  /// This can be used by high-frequency mutation sources (e.g. HTML parsing) to avoid doing extra
  /// work (like computing UTF-16 code unit lengths) when no live objects exist.
  #[inline]
  pub(crate) fn has_subscribers(&self) -> bool {
    self.hook.is_some() || !self.live_ranges.is_empty() || !self.node_iterators.is_empty()
  }

  pub(crate) fn set_hook(&mut self, hook: Option<Box<dyn LiveMutationHook>>) {
    self.hook = hook;
  }

  pub(crate) fn alloc_live_range_id(&mut self) -> LiveRangeId {
    let id = LiveRangeId(self.next_live_range_id);
    self.next_live_range_id = self.next_live_range_id.wrapping_add(1);
    id
  }

  pub(crate) fn register_live_range(&mut self, heap: &Heap, wrapper: GcObject) -> LiveRangeId {
    // Note: sweeping is performed by `Document::sweep_dead_live_traversals_if_needed` so the
    // document can also clean up any Rust-side traversal state (e.g. NodeIterator state) associated
    // with collected wrappers.
    let _ = heap;
    let id = self.alloc_live_range_id();
    self.live_ranges.insert(id, WeakGcObject::from(wrapper));
    id
  }

  /// Register a JS `NodeIterator` wrapper object for an existing `NodeIteratorId`.
  ///
  /// The id itself is allocated by `Document::create_node_iterator`, which also creates the
  /// iterator's Rust-side traversal state. This registry only tracks the JS wrapper weakly so we
  /// can later sweep entries for collected JS objects without keeping them alive.
  pub(crate) fn register_node_iterator(
    &mut self,
    heap: &Heap,
    id: NodeIteratorId,
    wrapper: GcObject,
  ) {
    // See `register_live_range` for why we don't sweep here.
    let _ = heap;
    self.node_iterators.insert(id, WeakGcObject::from(wrapper));
  }

  pub(crate) fn sweep_dead_if_needed(&mut self, heap: &Heap) -> LiveMutationSweepResult {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return LiveMutationSweepResult::default();
    }
    self.last_gc_runs = gc_runs;

    let mut dead_live_ranges: Vec<LiveRangeId> = Vec::new();
    self.live_ranges.retain(|id, weak| {
      let alive = weak.upgrade(heap).is_some();
      if !alive {
        dead_live_ranges.push(*id);
      }
      alive
    });
    let mut dead_node_iterators: Vec<NodeIteratorId> = Vec::new();
    self.node_iterators.retain(|id, weak| {
      let alive = weak.upgrade(heap).is_some();
      if !alive {
        dead_node_iterators.push(*id);
      }
      alive
    });
    LiveMutationSweepResult {
      dead_live_ranges,
      dead_node_iterators,
    }
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

  #[cfg(test)]
  pub(crate) fn node_iterator_wrapper_len(&self) -> usize {
    self.node_iterators.len()
  }
}

impl Document {
  pub(crate) fn live_mutation_pre_insert(&mut self, parent: NodeId, index: usize, count: usize) {
    self.live_mutation.pre_insert(parent, index, count);
  }

  pub(crate) fn live_mutation_pre_remove(
    &mut self,
    node: NodeId,
    old_parent: NodeId,
    old_index: usize,
  ) {
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
    // Ensure we don't accumulate stale weak entries or traversal state across GC cycles.
    self.sweep_dead_live_traversals_if_needed(heap);
    let id = self.live_mutation.register_live_range(heap, wrapper);
    // JS-created live ranges start with both boundary points at (document, 0), matching
    // `Document.createRange()` in the DOM Standard.
    self.create_range_for_id(id);
    id
  }

  /// Register a JS `Range` wrapper object for an existing `RangeId`.
  ///
  /// This is used by binding layers that allocate the range id/state first (via `create_range`) and
  /// then construct the wrapper object.
  pub(crate) fn register_live_range_wrapper_for_id(
    &mut self,
    heap: &Heap,
    id: LiveRangeId,
    wrapper: GcObject,
  ) {
    self.sweep_dead_live_traversals_if_needed(heap);
    self
      .live_mutation
      .live_ranges
      .insert(id, WeakGcObject::from(wrapper));
  }

  /// Returns all live JS `Range` wrapper objects whose boundary point container nodes are present
  /// in `mapping`.
  ///
  /// This is useful for cross-document adoption between *alias* document wrappers that share the
  /// same underlying `dom2::Document`: node wrapper objects are rebound to a different JS
  /// `Document` wrapper, so `Range` wrappers must be rebound as well to keep `startContainer` /
  /// `endContainer` stable.
  pub(crate) fn live_range_wrappers_with_endpoints_in_mapping(
    &mut self,
    heap: &Heap,
    mapping: &HashMap<NodeId, NodeId>,
  ) -> Vec<GcObject> {
    if mapping.is_empty() {
      return Vec::new();
    }
    self.sweep_dead_live_traversals_if_needed(heap);

    let ids: Vec<LiveRangeId> = self.live_mutation.live_ranges.keys().copied().collect();
    let mut out: Vec<GcObject> = Vec::new();
    for id in ids {
      let Some(range) = self.ranges.get(&id) else {
        continue;
      };
      if !mapping.contains_key(&range.start.node) || !mapping.contains_key(&range.end.node) {
        continue;
      }
      let Some(weak) = self.live_mutation.live_ranges.get(&id).copied() else {
        continue;
      };
      if let Some(obj) = weak.upgrade(heap) {
        out.push(obj);
      }
    }
    out
  }

  /// Transfer live range state for any live ranges whose boundary points remain inside a subtree
  /// being adopted into `dest`.
  ///
  /// `mapping` is the old→new `NodeId` remap produced by the adoption algorithm.
  ///
  /// Each moved range receives a new `RangeId` allocated in `dest`; the returned `MovedLiveRange`
  /// entries allow the embedding layer to update wrapper host slots accordingly.
  pub(crate) fn move_live_ranges_to_after_node_id_remap(
    &mut self,
    heap: &Heap,
    dest: &mut Document,
    mapping: &HashMap<NodeId, NodeId>,
  ) -> Vec<MovedLiveRange> {
    if mapping.is_empty() || self.live_mutation.live_ranges.is_empty() {
      return Vec::new();
    }

    // Prune any stale weak wrappers up-front so upgrades below are expected to succeed.
    self.sweep_dead_live_traversals_if_needed(heap);
    dest.sweep_dead_live_traversals_if_needed(heap);

    let ids: Vec<LiveRangeId> = self.live_mutation.live_ranges.keys().copied().collect();
    let mut moved: Vec<MovedLiveRange> = Vec::new();

    for old_id in ids {
      let Some(range) = self.ranges.get(&old_id) else {
        continue;
      };
      // Only move ranges whose endpoints are fully inside the adopted subtree. Cross-root ranges
      // should have been collapsed by pre-remove steps when applicable.
      if !mapping.contains_key(&range.start.node) || !mapping.contains_key(&range.end.node) {
        continue;
      }

      let Some(weak) = self.live_mutation.live_ranges.remove(&old_id) else {
        continue;
      };
      let Some(wrapper) = weak.upgrade(heap) else {
        // Wrapper is gone; drop the Rust-side range state rather than migrating it.
        self.ranges.remove(&old_id);
        continue;
      };

      let Some(mut range_state) = self.ranges.remove(&old_id) else {
        continue;
      };

      if let Some(&new_start) = mapping.get(&range_state.start.node) {
        range_state.start.node = new_start;
      }
      if let Some(&new_end) = mapping.get(&range_state.end.node) {
        range_state.end.node = new_end;
      }

      // Allocate a fresh id in the destination document to avoid collisions with its own range id
      // space. The JS embedding layer updates wrapper host slots to preserve wrapper identity.
      let mut new_id = dest.live_mutation.alloc_live_range_id();
      while dest.ranges.contains_key(&new_id) || dest.live_mutation.live_ranges.contains_key(&new_id) {
        new_id = dest.live_mutation.alloc_live_range_id();
      }

      let prev = dest.ranges.insert(new_id, range_state);
      debug_assert!(prev.is_none(), "range id collision while moving live ranges");
      dest.live_mutation.live_ranges.insert(new_id, weak);

      moved.push(MovedLiveRange {
        wrapper,
        old_id,
        new_id,
      });
    }

    moved
  }

  pub(crate) fn register_node_iterator_wrapper(
    &mut self,
    heap: &Heap,
    id: NodeIteratorId,
    wrapper: GcObject,
  ) {
    // Registration is a convenient point to prune any previously collected wrappers and their
    // associated Rust-side traversal state, without requiring callers to explicitly call the sweep
    // API first.
    self.sweep_dead_live_traversals_if_needed(heap);
    self.live_mutation.register_node_iterator(heap, id, wrapper);
  }

  pub(crate) fn sweep_dead_live_traversals_if_needed(&mut self, heap: &Heap) {
    let sweep = self.live_mutation.sweep_dead_if_needed(heap);
    // Prune Rust-side Range traversal state for JS-collected ranges. This prevents stale range state
    // from accumulating across GC cycles.
    for id in sweep.dead_live_ranges {
      self.remove_range(id);
    }
    // Prune Rust-side NodeIterator traversal state for JS-collected iterators. This prevents stale
    // iterator state from accumulating across GC cycles.
    for id in sweep.dead_node_iterators {
      self.remove_node_iterator(id);
    }
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
    self.events.borrow_mut().push(LiveMutationEvent::PreInsert {
      parent,
      index,
      count,
    });
  }

  fn pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize) {
    self.events.borrow_mut().push(LiveMutationEvent::PreRemove {
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
