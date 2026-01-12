use super::{Document, DomError, NodeId, NodeKind};

#[inline]
fn name_matches(existing: &str, query: &str, is_html: bool) -> bool {
  if is_html {
    existing.eq_ignore_ascii_case(query)
  } else {
    existing == query
  }
}

impl Document {
  pub fn get_attribute(&self, node: NodeId, name: &str) -> Result<Option<&str>, DomError> {
    let node = self.node_checked(node)?;
    let (namespace, attrs) = match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace.as_str(), attributes.as_slice()),
      _ => return Err(DomError::InvalidNodeType),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    Ok(
      attrs
        .iter()
        .find(|(k, _)| name_matches(k.as_str(), name, is_html))
        .map(|(_, v)| v.as_str()),
    )
  }

  pub fn has_attribute(&self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    let namespace = match &node.kind {
      NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => namespace.as_str(),
      _ => return Err(DomError::InvalidNodeType),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    let attrs = match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => {
        attributes.as_slice()
      }
      _ => return Err(DomError::InvalidNodeType),
    };
    Ok(
      attrs
        .iter()
        .any(|(k, _)| name_matches(k.as_str(), name, is_html)),
    )
  }

  pub fn attribute_names(&self, node: NodeId) -> Result<Vec<String>, DomError> {
    let node = self.node_checked(node)?;
    let (namespace, attrs) = match &node.kind {
      NodeKind::Element {
        namespace,
        attributes,
        ..
      }
      | NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => (namespace.as_str(), attributes.as_slice()),
      _ => return Err(DomError::InvalidNodeType),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    if is_html {
      Ok(attrs.iter().map(|(k, _)| k.to_ascii_lowercase()).collect())
    } else {
      Ok(attrs.iter().map(|(k, _)| k.clone()).collect())
    }
  }

  pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) -> Result<bool, DomError> {
    let node_id = node;
    let (is_html, is_script) = {
      let node = self.node_checked(node_id)?;
      match &node.kind {
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
          let is_html = self.is_html_case_insensitive_namespace(namespace);
          let is_script = is_html && tag_name.eq_ignore_ascii_case("script");
          (is_html, is_script)
        }
        NodeKind::Slot { namespace, .. } => {
          (self.is_html_case_insensitive_namespace(namespace), false)
        }
        _ => return Err(DomError::InvalidNodeType),
      }
    };

    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeType),
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
      let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
      self.record_attribute_mutation(node_id);
      self.bump_mutation_generation();
      let _ = self.queue_mutation_record_attributes(node_id, name, old_value);
    }

    Ok(changed)
  }

  pub fn remove_attribute(&mut self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node_id = node;
    let is_html = {
      let node = self.node_checked(node_id)?;
      match &node.kind {
        NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
          self.is_html_case_insensitive_namespace(namespace)
        }
        _ => return Err(DomError::InvalidNodeType),
      }
    };

    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeType),
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
      let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
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
      let (is_html, is_script) = {
        let node = self.node_checked(node_id)?;
        match &node.kind {
          NodeKind::Element {
            tag_name,
            namespace,
            ..
          } => {
            let is_html = self.is_html_case_insensitive_namespace(namespace);
            let is_script = is_html && tag_name.eq_ignore_ascii_case("script");
            (is_html, is_script)
          }
          NodeKind::Slot { namespace, .. } => {
            (self.is_html_case_insensitive_namespace(namespace), false)
          }
          _ => return Err(DomError::InvalidNodeType),
        }
      };

      let changed = {
        let node = self.node_checked_mut(node_id)?;
        let attrs = match &mut node.kind {
          NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
          _ => return Err(DomError::InvalidNodeType),
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
        let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
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
