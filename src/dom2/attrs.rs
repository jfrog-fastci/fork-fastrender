use crate::dom::HTML_NAMESPACE;

use super::{DomError, Document, NodeId, NodeKind};

#[inline]
fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

#[inline]
fn name_matches(existing: &str, query: &str, is_html: bool) -> bool {
  if is_html {
    existing.eq_ignore_ascii_case(query)
  } else {
    existing == query
  }
}

fn attrs_and_is_html(kind: &NodeKind) -> Option<(&Vec<(String, String)>, bool)> {
  match kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some((attributes, is_html_namespace(namespace))),
    _ => None,
  }
}

fn attrs_and_is_html_mut(kind: &mut NodeKind) -> Option<(&mut Vec<(String, String)>, bool)> {
  match kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some((attributes, is_html_namespace(namespace))),
    _ => None,
  }
}

#[inline]
fn is_html_script_element(kind: &NodeKind) -> bool {
  match kind {
    NodeKind::Element { tag_name, namespace, .. }
      if is_html_namespace(namespace) && tag_name.eq_ignore_ascii_case("script") =>
    {
      true
    }
    _ => false,
  }
}

impl Document {
  pub fn get_attribute(&self, node: NodeId, name: &str) -> Result<Option<&str>, DomError> {
    let node = self.node_checked(node)?;
    let Some((attrs, is_html)) = attrs_and_is_html(&node.kind) else {
      return Err(DomError::InvalidNodeType);
    };
    Ok(
      attrs
        .iter()
        .find(|(k, _)| name_matches(k.as_str(), name, is_html))
        .map(|(_, v)| v.as_str()),
    )
  }

  pub fn has_attribute(&self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    let Some((attrs, is_html)) = attrs_and_is_html(&node.kind) else {
      return Err(DomError::InvalidNodeType);
    };
    Ok(
      attrs
        .iter()
        .any(|(k, _)| name_matches(k.as_str(), name, is_html)),
    )
  }

  pub fn set_attribute(
    &mut self,
    node: NodeId,
    name: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    let node_id = node;
    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let is_script = is_html_script_element(&node.kind);
      let Some((attrs, is_html)) = attrs_and_is_html_mut(&mut node.kind) else {
        return Err(DomError::InvalidNodeType);
      };

      if let Some((_, existing)) = attrs
        .iter_mut()
        .find(|(k, _)| name_matches(k.as_str(), name, is_html))
      {
        if existing == value {
          return Ok(false);
        }
        let old_value = Some(existing.clone());
        existing.clear();
        existing.push_str(value);
        (true, old_value)
      } else {
        attrs.push((name.to_string(), value.to_string()));
        // HTML: adding the `async` attribute to a <script> clears the "force async" internal slot.
        if is_script && name.eq_ignore_ascii_case("async") {
          node.script_force_async = false;
        }
        (true, None)
      }
    };

    if changed {
      self.record_attribute_mutation(node_id);
      self.bump_mutation_generation();
      let _ = self.queue_mutation_record_attributes(node_id, name, old_value);
    }

    Ok(changed)
  }

  pub fn remove_attribute(&mut self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node_id = node;
    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let Some((attrs, is_html)) = attrs_and_is_html_mut(&mut node.kind) else {
        return Err(DomError::InvalidNodeType);
      };

      if let Some(idx) = attrs
        .iter()
        .position(|(k, _)| name_matches(k.as_str(), name, is_html))
      {
        let old_value = Some(attrs[idx].1.clone());
        attrs.remove(idx);
        (true, old_value)
      } else {
        (false, None)
      }
    };

    if changed {
      self.record_attribute_mutation(node_id);
      self.bump_mutation_generation();
      let _ = self.queue_mutation_record_attributes(node_id, name, old_value);
    }

    Ok(changed)
  }

  pub fn set_bool_attribute(
    &mut self,
    node: NodeId,
    name: &str,
    present: bool,
  ) -> Result<bool, DomError> {
    if present {
      let node_id = node;
      let changed = {
        let node = self.node_checked_mut(node_id)?;
        let is_script = is_html_script_element(&node.kind);
        let Some((attrs, is_html)) = attrs_and_is_html_mut(&mut node.kind) else {
          return Err(DomError::InvalidNodeType);
        };
        if attrs
          .iter()
          .any(|(k, _)| name_matches(k.as_str(), name, is_html))
        {
          false
        } else {
          attrs.push((name.to_string(), String::new()));
          // HTML: adding the `async` attribute to a <script> clears the "force async" internal slot.
          if is_script && name.eq_ignore_ascii_case("async") {
            node.script_force_async = false;
          }
          true
        }
      };
      if changed {
        self.record_attribute_mutation(node_id);
        self.bump_mutation_generation();
        let _ = self.queue_mutation_record_attributes(node_id, name, None);
      }

      Ok(changed)
    } else {
      self.remove_attribute(node, name)
    }
  }

  pub fn id(&self, node: NodeId) -> Result<Option<&str>, DomError> {
    self.get_attribute(node, "id")
  }

  pub fn class_name(&self, node: NodeId) -> Result<Option<&str>, DomError> {
    self.get_attribute(node, "class")
  }
}
