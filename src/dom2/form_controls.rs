use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, NodeId, NodeKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputState {
  pub(crate) value: String,
  pub(crate) dirty_value: bool,
  pub(crate) checkedness: bool,
  pub(crate) dirty_checkedness: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextareaState {
  pub(crate) value: String,
  pub(crate) dirty_value: bool,
}

#[inline]
fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

#[inline]
fn attrs_get_ci<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
  attrs
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

#[inline]
fn attrs_has_ci(attrs: &[(String, String)], name: &str) -> bool {
  attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

fn is_input_checkable(type_attr: Option<&str>) -> bool {
  let ty = type_attr.unwrap_or("text");
  ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio")
}

impl Document {
  pub(crate) fn init_form_control_state_for_node_kind(&self, kind: &NodeKind) -> (Option<InputState>, Option<TextareaState>) {
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
    } = kind
    else {
      return (None, None);
    };
    if !is_html_namespace(namespace) {
      return (None, None);
    }

    if tag_name.eq_ignore_ascii_case("input") {
      let value = attrs_get_ci(attributes, "value").unwrap_or("").to_string();
      let type_attr = attrs_get_ci(attributes, "type");
      let checkable = is_input_checkable(type_attr);
      let checkedness = checkable && attrs_has_ci(attributes, "checked");
      return (
        Some(InputState {
          value,
          dirty_value: false,
          checkedness,
          dirty_checkedness: false,
        }),
        None,
      );
    }

    if tag_name.eq_ignore_ascii_case("textarea") {
      return (
        None,
        Some(TextareaState {
          // The textarea default value depends on descendant text nodes; that can only be computed
          // reliably once the node is connected to its children. We keep the dirty flag here and
          // compute the default value on demand.
          value: String::new(),
          dirty_value: false,
        }),
      );
    }

    (None, None)
  }

  fn input_state(&self, node: NodeId) -> Result<&InputState, DomError> {
    self
      .input_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .ok_or(DomError::InvalidNodeType)
  }

  fn input_state_mut(&mut self, node: NodeId) -> Result<&mut InputState, DomError> {
    self
      .input_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeType)
  }

  fn textarea_state(&self, node: NodeId) -> Result<&TextareaState, DomError> {
    self
      .textarea_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .ok_or(DomError::InvalidNodeType)
  }

  fn textarea_state_mut(&mut self, node: NodeId) -> Result<&mut TextareaState, DomError> {
    self
      .textarea_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeType)
  }

  pub(crate) fn sync_form_control_state_after_attr_mutation(
    &mut self,
    node: NodeId,
    name: &str,
  ) -> Result<(), DomError> {
    let Some(state) = self
      .input_states
      .get(node.index())
      .and_then(|s| s.as_ref())
    else {
      return Ok(());
    };
    let dirty_value = state.dirty_value;
    let dirty_checkedness = state.dirty_checkedness;

    // Spec-ish: when the input's "dirty value flag" is false, mutations to the `value` content
    // attribute update the current value. Once dirty, the attribute only affects the default value
    // and the current value is preserved until form reset.
    if name.eq_ignore_ascii_case("value") && !dirty_value {
      let new_value = self.get_attribute(node, "value")?.unwrap_or("").to_string();
      if let Some(state) = self
        .input_states
        .get_mut(node.index())
        .and_then(|s| s.as_mut())
      {
        if !state.dirty_value {
          state.value = new_value;
        }
      }
    }

    // Spec-ish: checkedness only tracks the `checked` content attribute while the "dirty checkedness
    // flag" is false.
    //
    // Checkedness depends on the input type, so also recompute when `type` changes.
    if (name.eq_ignore_ascii_case("checked") || name.eq_ignore_ascii_case("type")) && !dirty_checkedness {
      let checkable = is_input_checkable(self.get_attribute(node, "type")?);
      let checked_attr = self.has_attribute(node, "checked")?;
      let checkedness = checkable && checked_attr;
      if let Some(state) = self
        .input_states
        .get_mut(node.index())
        .and_then(|s| s.as_mut())
      {
        if !state.dirty_checkedness {
          state.checkedness = checkedness;
        }
      }
    }

    Ok(())
  }

  pub fn input_value(&self, input: NodeId) -> Result<&str, DomError> {
    // Bounds check + validate node type.
    let _ = self.node_checked(input)?;
    Ok(self.input_state(input)?.value.as_str())
  }

  pub fn set_input_value(&mut self, input: NodeId, value: &str) -> Result<(), DomError> {
    let _ = self.node_checked(input)?;
    let state = self.input_state_mut(input)?;
    state.dirty_value = true;
    state.value.clear();
    state.value.push_str(value);
    Ok(())
  }

  pub fn input_checked(&self, input: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(input)?;
    Ok(self.input_state(input)?.checkedness)
  }

  pub fn set_input_checked(&mut self, input: NodeId, checked: bool) -> Result<(), DomError> {
    let _ = self.node_checked(input)?;
    let state = self.input_state_mut(input)?;
    state.dirty_checkedness = true;
    state.checkedness = checked;
    Ok(())
  }

  pub fn textarea_value(&self, textarea: NodeId) -> Result<String, DomError> {
    let _ = self.node_checked(textarea)?;
    let state = self.textarea_state(textarea)?;
    if state.dirty_value {
      return Ok(state.value.clone());
    }
    self.textarea_default_value(textarea)
  }

  pub fn set_textarea_value(&mut self, textarea: NodeId, value: &str) -> Result<(), DomError> {
    let _ = self.node_checked(textarea)?;
    let state = self.textarea_state_mut(textarea)?;
    state.dirty_value = true;
    state.value.clear();
    state.value.push_str(value);
    Ok(())
  }

  fn textarea_default_value(&self, textarea: NodeId) -> Result<String, DomError> {
    let node = self.node_checked(textarea)?;
    match &node.kind {
      NodeKind::Element { tag_name, namespace, .. }
        if is_html_namespace(namespace) && tag_name.eq_ignore_ascii_case("textarea") => {}
      _ => return Err(DomError::InvalidNodeType),
    }

    // Minimal default value: concatenate descendant text node data in tree order.
    let mut out = String::new();
    let mut stack: Vec<NodeId> = vec![textarea];
    while let Some(node_id) = stack.pop() {
      let node = self.node(node_id);
      match &node.kind {
        NodeKind::Text { content } => out.push_str(content),
        _ => {}
      }
      for &child in node.children.iter().rev() {
        if self.nodes.get(child.index()).is_some_and(|child_node| child_node.parent == Some(node_id)) {
          stack.push(child);
        }
      }
    }
    // Remove the textarea element itself from consideration: it is never a text node, so the first
    // iteration is effectively a no-op.
    Ok(out)
  }

  pub fn form_reset(&mut self, form: NodeId) -> Result<(), DomError> {
    let node = self.node_checked(form)?;
    match &node.kind {
      NodeKind::Element { tag_name, namespace, .. }
        if is_html_namespace(namespace) && tag_name.eq_ignore_ascii_case("form") => {}
      _ => return Err(DomError::InvalidNodeType),
    }

    // Minimal: reset descendant <input> and <textarea> elements.
    let mut stack: Vec<NodeId> = vec![form];
    while let Some(node_id) = stack.pop() {
      if self
        .input_states
        .get(node_id.index())
        .and_then(|s| s.as_ref())
        .is_some()
      {
        let default_value = self.get_attribute(node_id, "value")?.unwrap_or("").to_string();
        let checkable = is_input_checkable(self.get_attribute(node_id, "type")?);
        let checked_attr = self.has_attribute(node_id, "checked")?;
        let default_checkedness = checkable && checked_attr;
        if let Some(state) = self
          .input_states
          .get_mut(node_id.index())
          .and_then(|s| s.as_mut())
        {
          state.dirty_value = false;
          state.value = default_value;
          state.dirty_checkedness = false;
          state.checkedness = default_checkedness;
        }
      }

      if self
        .textarea_states
        .get(node_id.index())
        .and_then(|s| s.as_ref())
        .is_some()
      {
        let default_value = self.textarea_default_value(node_id)?;
        if let Some(state) = self
          .textarea_states
          .get_mut(node_id.index())
          .and_then(|s| s.as_mut())
        {
          state.dirty_value = false;
          state.value = default_value;
        }
      }

      // Descend.
      let children = self.node(node_id).children.clone();
      for child in children.into_iter().rev() {
        if self.nodes.get(child.index()).is_some_and(|child_node| child_node.parent == Some(node_id)) {
          stack.push(child);
        }
      }
    }

    Ok(())
  }
}
