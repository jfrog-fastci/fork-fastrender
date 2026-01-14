use crate::dom::DomNode;
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::ptr;

/// Ephemeral DOM index for interaction work (hit testing, form control updates, etc).
///
/// # Invalidation
/// This index stores raw pointers into the `DomNode` tree. It is valid only as long as the DOM
/// structure is unchanged (i.e. no edits that can reallocate `children` vectors). Callers must
/// rebuild the index after structural edits.
#[derive(Debug)]
pub struct DomIndex {
  /// Parent id for each node (1-based ids; `0` means no parent/root).
  pub parent: Vec<usize>,
  /// Mutable pointer for each id (index is the 1-based id; entry 0 is null).
  ///
  /// Safety: these pointers are only valid while the underlying DOM structure is unchanged.
  id_to_ptr: Vec<*mut DomNode>,
  /// Mapping from element `id` attribute value to node id.
  ///
  /// This is primarily intended for future `<label for=...>` association and other ID based lookups.
  pub id_by_element_id: HashMap<String, usize>,
  /// Mapping from node pointer back to its 1-based pre-order id.
  ///
  /// This is used by hit-testing features such as `<img usemap>` that need to resolve a returned
  /// `<area>` pointer back to the DOM id used by the renderer/interaction engine.
  ptr_to_id: FxHashMap<*const DomNode, usize>,
}

impl DomIndex {
  #[must_use]
  pub fn build(root: &mut DomNode) -> Self {
    // Pre-order traversal; id 1 is always the root.
    let mut parent: Vec<usize> = vec![0];
    let mut id_to_ptr: Vec<*mut DomNode> = vec![ptr::null_mut()];
    let mut id_by_element_id: HashMap<String, usize> = HashMap::new();
    let mut ptr_to_id: FxHashMap<*const DomNode, usize> = FxHashMap::default();

    // Track whether a node is inside an inert `<template>` subtree. `enumerate_dom_ids` includes
    // template contents in the stable node id scheme, but template contents should not participate
    // in `id` attribute lookup (matching browser `getElementById` behaviour).
    let mut stack: Vec<(*mut DomNode, usize, bool)> = vec![(root as *mut DomNode, 0, false)];
    while let Some((ptr, parent_id, in_template_contents)) = stack.pop() {
      let id = id_to_ptr.len();
      id_to_ptr.push(ptr);
      parent.push(parent_id);
      debug_assert!(!ptr.is_null());
      ptr_to_id.insert(ptr as *const DomNode, id);

      // Safety: `root` is mutably borrowed for the duration of `build`, and we only traverse the
      // existing structure without mutating any `children` vectors, so raw pointers are stable for
      // this walk.
      let node = unsafe { &mut *ptr };

      if !in_template_contents {
        if let Some(element_id) = node.get_attribute_ref("id") {
          // Keep the first occurrence to match typical getElementById behavior.
          id_by_element_id.entry(element_id.to_string()).or_insert(id);
        }
      }

      let child_in_template_contents = in_template_contents || node.is_template_element();
      for child in node.children.iter_mut().rev() {
        stack.push((child as *mut DomNode, id, child_in_template_contents));
      }
    }

    Self {
      parent,
      id_to_ptr,
      id_by_element_id,
      ptr_to_id,
    }
  }

  #[must_use]
  pub fn len(&self) -> usize {
    self.id_to_ptr.len().saturating_sub(1)
  }

  pub fn node(&self, id: usize) -> Option<&DomNode> {
    let ptr = *self.id_to_ptr.get(id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` points into the DOM tree used to build this index. The index is only valid as
    // long as callers don't structurally edit the DOM (reallocating `children` vectors).
    Some(unsafe { &*ptr })
  }

  pub fn node_mut(&mut self, id: usize) -> Option<&mut DomNode> {
    let ptr = *self.id_to_ptr.get(id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` points into the DOM tree used to build this index. The index is only valid as
    // long as callers don't structurally edit the DOM (reallocating `children` vectors).
    Some(unsafe { &mut *ptr })
  }

  pub fn with_node_mut<R>(&mut self, id: usize, f: impl FnOnce(&mut DomNode) -> R) -> Option<R> {
    let ptr = *self.id_to_ptr.get(id)?;
    if ptr.is_null() {
      return None;
    }
    // Safety: the index guarantees `ptr` is a node pointer in the tree as built, and this method
    // hides the raw pointer dereference from callers.
    Some(f(unsafe { &mut *ptr }))
  }

  /// Resolve a raw pointer back to its 1-based DOM pre-order id.
  #[inline]
  pub fn id_for_ptr(&self, ptr: *const DomNode) -> Option<usize> {
    if ptr.is_null() {
      return None;
    }
    self.ptr_to_id.get(&ptr).copied()
  }
}
