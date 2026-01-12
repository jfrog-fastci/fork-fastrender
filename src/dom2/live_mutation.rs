use super::NodeId;

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
  /// Lengths are measured in Rust string bytes (`String::len()`), not UTF-16 code units.
  fn replace_data(&mut self, node: NodeId, offset: usize, removed_len: usize, inserted_len: usize);
}

#[derive(Default)]
pub(crate) struct LiveMutation {
  hook: Option<Box<dyn LiveMutationHook>>,
}

impl LiveMutation {
  pub(crate) fn set_hook(&mut self, hook: Option<Box<dyn LiveMutationHook>>) {
    self.hook = hook;
  }

  #[inline]
  pub(crate) fn pre_insert(&mut self, parent: NodeId, index: usize, count: usize) {
    if let Some(hook) = self.hook.as_mut() {
      hook.pre_insert(parent, index, count);
    }
  }

  #[inline]
  pub(crate) fn pre_remove(&mut self, node: NodeId, old_parent: NodeId, old_index: usize) {
    if let Some(hook) = self.hook.as_mut() {
      hook.pre_remove(node, old_parent, old_index);
    }
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

