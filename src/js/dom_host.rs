use crate::dom2::{DomError, Document, NodeId};

/// Abstraction over a live `dom2::Document` that allows DOM mutation while keeping renderer cache
/// invalidation coalesced in the host.
///
/// JS bindings should **not** own the document directly (e.g. `Rc<RefCell<Document>>`), because
/// that would bypass host invalidation hooks and lead to stale renders.
///
/// Instead, DOM bindings should route all mutations through [`DomHost::mutate_dom`] and report
/// whether the DOM actually changed. The host can then invalidate style/layout/paint caches only
/// when needed.
pub trait DomHost {
  /// Borrow the live DOM immutably.
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R;

  /// Mutate the live DOM and report whether anything changed.
  ///
  /// The closure returns `(result, changed)`.
  ///
  /// Hosts should only invalidate renderer caches when `changed == true`.
  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool);
}

/// Object-safe DOM host operations needed by `vm-js` DOM shims.
///
/// `DomHost` intentionally uses generic closures to support returning arbitrary values from
/// `with_dom`/`mutate_dom`, but that also makes it **not** dyn-compatible. The `vm-js` embedding needs
/// to store host pointers in a thread-local registry (keyed by `dom_source_id`) and therefore needs
/// a dyn-compatible surface.
///
/// This trait provides that surface by exposing only the concrete DOM operations used by the shims
/// (dataset/classList/style, plus node-id decoding).
pub trait DomHostVmJs {
  fn node_id_from_index(&self, node_index: usize) -> Result<NodeId, DomError>;

  fn set_element_class_name(&mut self, element: NodeId, value: &str) -> Result<bool, DomError>;

  fn class_list_add(&mut self, element: NodeId, tokens: &[&str]) -> Result<bool, DomError>;
  fn class_list_remove(&mut self, element: NodeId, tokens: &[&str]) -> Result<bool, DomError>;
  fn class_list_toggle(
    &mut self,
    element: NodeId,
    token: &str,
    force: Option<bool>,
  ) -> Result<bool, DomError>;
  fn class_list_replace(&mut self, element: NodeId, token: &str, new_token: &str) -> Result<bool, DomError>;

  fn dataset_get(&self, element: NodeId, prop: &str) -> Option<String>;
  fn dataset_set(&mut self, element: NodeId, prop: &str, value: &str) -> Result<bool, DomError>;
  fn dataset_delete(&mut self, element: NodeId, prop: &str) -> Result<bool, DomError>;

  fn style_get_property_value(&self, element: NodeId, name: &str) -> String;
  fn style_set_property(&mut self, element: NodeId, name: &str, value: &str) -> Result<bool, DomError>;
}

impl<T> DomHostVmJs for T
where
  T: DomHost,
{
  fn node_id_from_index(&self, node_index: usize) -> Result<NodeId, DomError> {
    self.with_dom(|dom| dom.node_id_from_index(node_index))
  }

  fn set_element_class_name(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.set_element_class_name(element, value) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }

  fn class_list_add(&mut self, element: NodeId, tokens: &[&str]) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.class_list_add(element, tokens) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }

  fn class_list_remove(&mut self, element: NodeId, tokens: &[&str]) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.class_list_remove(element, tokens) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }

  fn class_list_toggle(
    &mut self,
    element: NodeId,
    token: &str,
    force: Option<bool>,
  ) -> Result<bool, DomError> {
    self.mutate_dom(|dom| {
      let before = match dom.class_list_contains(element, token) {
        Ok(v) => v,
        Err(err) => return (Err(err), false),
      };
      match dom.class_list_toggle(element, token, force) {
        Ok(after) => {
          let changed = after != before;
          (Ok(after), changed)
        }
        Err(err) => (Err(err), false),
      }
    })
  }

  fn class_list_replace(&mut self, element: NodeId, token: &str, new_token: &str) -> Result<bool, DomError> {
    self.mutate_dom(|dom| {
      let before = match dom.get_attribute(element, "class") {
        Ok(v) => v.map(str::to_string),
        Err(err) => return (Err(err), false),
      };
      match dom.class_list_replace(element, token, new_token) {
        Ok(found) => {
          let after = match dom.get_attribute(element, "class") {
            Ok(v) => v.map(str::to_string),
            Err(err) => return (Err(err), false),
          };
          let changed = before != after;
          (Ok(found), changed)
        }
        Err(err) => (Err(err), false),
      }
    })
  }

  fn dataset_get(&self, element: NodeId, prop: &str) -> Option<String> {
    self.with_dom(|dom| dom.dataset_get(element, prop).map(str::to_string))
  }

  fn dataset_set(&mut self, element: NodeId, prop: &str, value: &str) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.dataset_set(element, prop, value) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }

  fn dataset_delete(&mut self, element: NodeId, prop: &str) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.dataset_delete(element, prop) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }

  fn style_get_property_value(&self, element: NodeId, name: &str) -> String {
    self.with_dom(|dom| dom.style_get_property_value(element, name))
  }

  fn style_set_property(&mut self, element: NodeId, name: &str, value: &str) -> Result<bool, DomError> {
    self.mutate_dom(|dom| match dom.style_set_property(element, name, value) {
      Ok(changed) => (Ok(changed), changed),
      Err(err) => (Err(err), false),
    })
  }
}
