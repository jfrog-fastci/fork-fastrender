use super::{Attribute, Document, DomError, NodeId, NodeKind, NULL_NAMESPACE};

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
      _ => return Err(DomError::InvalidNodeTypeError),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    Ok(
      attrs
        .iter()
        .find(|attr| attr.qualified_name_matches(name, is_html))
        .map(|attr| attr.value.as_str()),
    )
  }

  pub fn has_attribute(&self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node = self.node_checked(node)?;
    let namespace = match &node.kind {
      NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => namespace.as_str(),
      _ => return Err(DomError::InvalidNodeTypeError),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    let attrs = match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => {
        attributes.as_slice()
      }
      _ => return Err(DomError::InvalidNodeTypeError),
    };
    Ok(
      attrs
        .iter()
        .any(|attr| attr.qualified_name_matches(name, is_html)),
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
      _ => return Err(DomError::InvalidNodeTypeError),
    };
    let is_html = self.is_html_case_insensitive_namespace(namespace);
    if is_html {
      Ok(
        attrs
          .iter()
          .map(|attr| attr.qualified_name().to_ascii_lowercase())
          .collect(),
      )
    } else {
      Ok(attrs.iter().map(|attr| attr.qualified_name().into_owned()).collect())
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
        _ => return Err(DomError::InvalidNodeTypeError),
      }
    };

    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeTypeError),
      };

      if let Some(existing) = attrs
        .iter_mut()
        .find(|attr| attr.namespace == NULL_NAMESPACE && name_matches(attr.local_name.as_str(), name, is_html))
      {
        if existing.value == value {
          return Ok(false);
        }
        let old_value = Some(existing.value.clone());
        existing.value.clear();
        existing.value.push_str(value);
        (true, old_value)
      } else {
        attrs.push(Attribute::new_no_namespace(name, value));
        // HTML: adding the `async` attribute to a <script> clears the "force async" internal slot.
        if is_script && name.eq_ignore_ascii_case("async") {
          node.script_force_async = false;
        }
        (true, None)
      }
    };

    if changed {
      let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
      self.record_attribute_mutation(node_id, name);
      self.bump_mutation_generation_classified();
      let _ = self.queue_mutation_record_attributes(node_id, name, old_value);

      // Slot-related attributes can affect shadow DOM distribution without changing the DOM tree
      // structure. Recompute derived slotting state and record a composed-tree mutation so
      // incremental hosts can invalidate correctly.
      if self.is_html_document() {
        if name.eq_ignore_ascii_case("slot") {
          if let Some(parent) = self.node(node_id).parent {
            if let Some(shadow_root) = self.shadow_root_for_host(parent) {
              self.assign_slottables_for_tree(shadow_root);
              self.record_composed_tree_mutation(shadow_root);
            }
          }
        }

        if matches!(self.node(node_id).kind, NodeKind::Slot { .. }) && name.eq_ignore_ascii_case("name") {
          if let Some(shadow_root) = self.shadow_root_ancestor(node_id) {
            self.assign_slottables_for_tree(shadow_root);
            self.record_composed_tree_mutation(shadow_root);
          }
        }
      }
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
        _ => return Err(DomError::InvalidNodeTypeError),
      }
    };

    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeTypeError),
      };

      if let Some(idx) = attrs
        .iter()
        .position(|attr| attr.namespace == NULL_NAMESPACE && name_matches(attr.local_name.as_str(), name, is_html))
      {
        let old_value = Some(attrs[idx].value.clone());
        attrs.remove(idx);
        (true, old_value)
      } else {
        (false, None)
      }
    };

    if changed {
      let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
      self.record_attribute_mutation(node_id, name);
      self.bump_mutation_generation_classified();
      let _ = self.queue_mutation_record_attributes(node_id, name, old_value);

      if self.is_html_document() {
        if name.eq_ignore_ascii_case("slot") {
          if let Some(parent) = self.node(node_id).parent {
            if let Some(shadow_root) = self.shadow_root_for_host(parent) {
              self.assign_slottables_for_tree(shadow_root);
              self.record_composed_tree_mutation(shadow_root);
            }
          }
        }

        if matches!(self.node(node_id).kind, NodeKind::Slot { .. }) && name.eq_ignore_ascii_case("name") {
          if let Some(shadow_root) = self.shadow_root_ancestor(node_id) {
            self.assign_slottables_for_tree(shadow_root);
            self.record_composed_tree_mutation(shadow_root);
          }
        }
      }
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
          _ => return Err(DomError::InvalidNodeTypeError),
        }
      };

      let changed = {
        let node = self.node_checked_mut(node_id)?;
        let attrs = match &mut node.kind {
          NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
          _ => return Err(DomError::InvalidNodeTypeError),
        };
        if attrs.iter().any(|attr| {
          attr.namespace == NULL_NAMESPACE && name_matches(attr.local_name.as_str(), name, is_html)
        }) {
          false
        } else {
          attrs.push(Attribute::new_no_namespace(name, ""));
          // HTML: adding the `async` attribute to a <script> clears the "force async" internal slot.
          if is_script && name.eq_ignore_ascii_case("async") {
            node.script_force_async = false;
          }
          true
        }
      };
      if changed {
        let _ = self.sync_form_control_state_after_attr_mutation(node_id, name);
        self.record_attribute_mutation(node_id, name);
        self.bump_mutation_generation_classified();
        let _ = self.queue_mutation_record_attributes(node_id, name, None);
      }

      Ok(changed)
    } else {
      self.remove_attribute(node, name)
    }
  }

  pub fn get_attribute_ns(
    &self,
    node: NodeId,
    namespace: &str,
    local_name: &str,
  ) -> Result<Option<&str>, DomError> {
    let node = self.node_checked(node)?;
    let (element_ns, attrs) = match &node.kind {
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
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    let is_html = self.is_html_case_insensitive_namespace(element_ns);
    let ci = namespace == NULL_NAMESPACE && is_html;
    Ok(
      attrs
        .iter()
        .find(|attr| {
          attr.namespace == namespace && name_matches(attr.local_name.as_str(), local_name, ci)
        })
        .map(|attr| attr.value.as_str()),
    )
  }

  pub fn set_attribute_ns(
    &mut self,
    node: NodeId,
    namespace: &str,
    prefix: Option<&str>,
    local_name: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    let node_id = node;
    let (is_html, is_script) = {
      let node = self.node_checked(node_id)?;
      match &node.kind {
        NodeKind::Element {
          tag_name,
          namespace: element_ns,
          ..
        } => {
          let is_html = self.is_html_case_insensitive_namespace(element_ns);
          let is_script = is_html && tag_name.eq_ignore_ascii_case("script");
          (is_html, is_script)
        }
        NodeKind::Slot { namespace: element_ns, .. } => {
          (self.is_html_case_insensitive_namespace(element_ns), false)
        }
        _ => return Err(DomError::InvalidNodeTypeError),
      }
    };

    let ci = namespace == NULL_NAMESPACE && is_html;
    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeTypeError),
      };

      if let Some(existing) = attrs.iter_mut().find(|attr| {
        attr.namespace == namespace && name_matches(attr.local_name.as_str(), local_name, ci)
      }) {
        if existing.value == value && existing.prefix.as_deref() == prefix {
          return Ok(false);
        }
        let old_value = Some(existing.value.clone());
        existing.value.clear();
        existing.value.push_str(value);
        existing.prefix = prefix.map(|p| p.to_string());
        (true, old_value)
      } else {
        attrs.push(Attribute::new(namespace, prefix, local_name, value));
        // HTML: adding the `async` attribute to a <script> clears the "force async" internal slot.
        if namespace == NULL_NAMESPACE && is_script && local_name.eq_ignore_ascii_case("async") {
          node.script_force_async = false;
        }
        (true, None)
      }
    };

    if changed {
      if namespace == NULL_NAMESPACE {
        let _ = self.sync_form_control_state_after_attr_mutation(node_id, local_name);
      }
      self.record_attribute_mutation(node_id, local_name);
      self.bump_mutation_generation_classified();
      let _ = self.queue_mutation_record_attributes(node_id, local_name, old_value);
    }

    Ok(changed)
  }

  pub fn remove_attribute_ns(
    &mut self,
    node: NodeId,
    namespace: &str,
    local_name: &str,
  ) -> Result<bool, DomError> {
    let node_id = node;
    let is_html = {
      let node = self.node_checked(node_id)?;
      match &node.kind {
        NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => {
          self.is_html_case_insensitive_namespace(namespace)
        }
        _ => return Err(DomError::InvalidNodeTypeError),
      }
    };

    let ci = namespace == NULL_NAMESPACE && is_html;
    let (changed, old_value) = {
      let node = self.node_checked_mut(node_id)?;
      let attrs = match &mut node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => return Err(DomError::InvalidNodeTypeError),
      };

      if let Some(idx) = attrs.iter().position(|attr| {
        attr.namespace == namespace && name_matches(attr.local_name.as_str(), local_name, ci)
      }) {
        let old_value = Some(attrs[idx].value.clone());
        attrs.remove(idx);
        (true, old_value)
      } else {
        (false, None)
      }
    };

    if changed {
      if namespace == NULL_NAMESPACE {
        let _ = self.sync_form_control_state_after_attr_mutation(node_id, local_name);
      }
      self.record_attribute_mutation(node_id, local_name);
      self.bump_mutation_generation_classified();
      let _ = self.queue_mutation_record_attributes(node_id, local_name, old_value);
    }

    Ok(changed)
  }

  pub fn id(&self, node: NodeId) -> Result<Option<&str>, DomError> {
    self.get_attribute(node, "id")
  }

  pub fn class_name(&self, node: NodeId) -> Result<Option<&str>, DomError> {
    self.get_attribute(node, "class")
  }
}
