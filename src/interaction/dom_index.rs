use crate::dom::DomNode;
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
}

impl DomIndex {
  #[must_use]
  pub fn build(root: &mut DomNode) -> Self {
    // Pre-order traversal; id 1 is always the root.
    let mut parent: Vec<usize> = vec![0];
    let mut id_to_ptr: Vec<*mut DomNode> = vec![ptr::null_mut()];
    let mut id_by_element_id: HashMap<String, usize> = HashMap::new();

    let mut stack: Vec<(*mut DomNode, usize)> = vec![(root as *mut DomNode, 0)];
    while let Some((ptr, parent_id)) = stack.pop() {
      let id = id_to_ptr.len();
      id_to_ptr.push(ptr);
      parent.push(parent_id);

      // Safety: `root` is mutably borrowed for the duration of `build`, and we only traverse the
      // existing structure without mutating any `children` vectors, so raw pointers are stable for
      // this walk.
      let node = unsafe { &mut *ptr };

      if let Some(element_id) = node.get_attribute_ref("id") {
        // Keep the first occurrence to match typical getElementById behavior.
        id_by_element_id
          .entry(element_id.to_string())
          .or_insert(id);
      }

      if node.is_template_element() {
        continue;
      }
      for child in node.children.iter_mut().rev() {
        stack.push((child as *mut DomNode, id));
      }
    }

    Self {
      parent,
      id_to_ptr,
      id_by_element_id,
    }
  }

  #[must_use]
  pub fn len(&self) -> usize {
    self.id_to_ptr.len().saturating_sub(1)
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
}

