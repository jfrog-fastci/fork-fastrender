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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OptionState {
  pub(crate) selectedness: bool,
  pub(crate) dirty_selectedness: bool,
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
  pub(crate) fn init_form_control_state_for_node_kind(
    &self,
    kind: &NodeKind,
  ) -> (Option<InputState>, Option<TextareaState>, Option<OptionState>) {
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } = kind
    else {
      return (None, None, None);
    };
    if !self.is_html_case_insensitive_namespace(namespace) {
      return (None, None, None);
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
        None,
      );
    }

    if tag_name.eq_ignore_ascii_case("option") {
      let selectedness = attrs_has_ci(attributes, "selected");
      return (
        None,
        None,
        Some(OptionState {
          selectedness,
          dirty_selectedness: false,
        }),
      );
    }

    (None, None, None)
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

  fn option_state(&self, node: NodeId) -> Result<&OptionState, DomError> {
    self
      .option_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .ok_or(DomError::InvalidNodeType)
  }

  fn option_state_mut(&mut self, node: NodeId) -> Result<&mut OptionState, DomError> {
    self
      .option_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeType)
  }

  pub(crate) fn sync_form_control_state_after_attr_mutation(
    &mut self,
    node: NodeId,
    name: &str,
  ) -> Result<(), DomError> {
    let input_dirty = self
      .input_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .map(|state| (state.dirty_value, state.dirty_checkedness));

    // Spec-ish: when the input's "dirty value flag" is false, mutations to the `value` content
    // attribute update the current value. Once dirty, the attribute only affects the default value
    // and the current value is preserved until form reset.
    if let Some((dirty_value, dirty_checkedness)) = input_dirty {
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
    }

    let option_dirty = self
      .option_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .map(|state| state.dirty_selectedness);
    if let Some(dirty_selectedness) = option_dirty {
      // Spec-ish: when the option's dirty selectedness flag is false, the selectedness follows the
      // `selected` content attribute.
      if name.eq_ignore_ascii_case("selected") && !dirty_selectedness {
        let selectedness = self.has_attribute(node, "selected")?;
        if let Some(state) = self
          .option_states
          .get_mut(node.index())
          .and_then(|s| s.as_mut())
        {
          if !state.dirty_selectedness {
            state.selectedness = selectedness;
          }
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
    self.bump_mutation_generation();
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
    self.bump_mutation_generation();
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
    self.bump_mutation_generation();
    Ok(())
  }

  pub fn option_selected(&self, option: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(option)?;
    Ok(self.option_state(option)?.selectedness)
  }

  pub fn set_option_selected(&mut self, option: NodeId, selected: bool) -> Result<(), DomError> {
    let _ = self.node_checked(option)?;

    {
      let state = self.option_state_mut(option)?;
      state.dirty_selectedness = true;
      state.selectedness = selected;
    }
    self.bump_mutation_generation();

    let Some(select) = self
      .ancestors(option)
      .skip(1)
      .find(|&ancestor| {
        let NodeKind::Element {
          tag_name,
          namespace,
          ..
        } = &self.node(ancestor).kind
        else {
          return false;
        };
        self.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("select")
      })
    else {
      return Ok(());
    };

    let multiple = self.has_attribute(select, "multiple")?;
    if multiple {
      return Ok(());
    }

    let options = self.select_options(select);
    if options.is_empty() {
      return Ok(());
    }

    if selected {
      for other in options {
        if other == option {
          continue;
        }
        let Some(state) = self
          .option_states
          .get_mut(other.index())
          .and_then(|s| s.as_mut())
        else {
          continue;
        };
        if state.selectedness {
          state.selectedness = false;
          state.dirty_selectedness = true;
          self.bump_mutation_generation();
        }
      }
      return Ok(());
    }

    let any_selected = options.iter().any(|&opt| {
      self
        .option_states
        .get(opt.index())
        .and_then(|s| s.as_ref())
        .is_some_and(|s| s.selectedness)
    });
    if any_selected {
      return Ok(());
    }

    let first = options[0];
    let Some(state) = self
      .option_states
      .get_mut(first.index())
      .and_then(|s| s.as_mut())
    else {
      return Ok(());
    };
    if !state.selectedness {
      state.selectedness = true;
      self.bump_mutation_generation();
    }
    Ok(())
  }

  pub fn reset_input(&mut self, input: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(input)?;
    let default_value = self.get_attribute(input, "value")?.unwrap_or("").to_string();
    let checkable = is_input_checkable(self.get_attribute(input, "type")?);
    let checked_attr = self.has_attribute(input, "checked")?;
    let default_checkedness = checkable && checked_attr;
    let state = self.input_state_mut(input)?;
    state.dirty_value = false;
    state.value = default_value;
    state.dirty_checkedness = false;
    state.checkedness = default_checkedness;
    self.bump_mutation_generation();
    Ok(())
  }

  pub fn reset_textarea(&mut self, textarea: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(textarea)?;
    let state = self.textarea_state_mut(textarea)?;
    state.dirty_value = false;
    state.value.clear();
    self.bump_mutation_generation();
    Ok(())
  }

  pub fn reset_option(&mut self, option: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(option)?;
    let selectedness = self.has_attribute(option, "selected")?;
    let state = self.option_state_mut(option)?;
    state.dirty_selectedness = false;
    state.selectedness = selectedness;
    self.bump_mutation_generation();
    Ok(())
  }

  fn textarea_default_value(&self, textarea: NodeId) -> Result<String, DomError> {
    let node = self.node_checked(textarea)?;
    match &node.kind {
      NodeKind::Element { tag_name, namespace, .. }
        if self.is_html_case_insensitive_namespace(namespace)
          && tag_name.eq_ignore_ascii_case("textarea") => {}
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
        if self.is_html_case_insensitive_namespace(namespace)
          && tag_name.eq_ignore_ascii_case("form") => {}
      _ => return Err(DomError::InvalidNodeType),
    }

    // Minimal: reset descendant <input>, <textarea>, and <option> elements.
    let mut stack: Vec<NodeId> = vec![form];
    let mut any = false;
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
          any = true;
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
          any = true;
        }
      }

      if self
        .option_states
        .get(node_id.index())
        .and_then(|s| s.as_ref())
        .is_some()
      {
        let selectedness = self.has_attribute(node_id, "selected")?;
        if let Some(state) = self
          .option_states
          .get_mut(node_id.index())
          .and_then(|s| s.as_mut())
        {
          state.dirty_selectedness = false;
          state.selectedness = selectedness;
          any = true;
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

    if any {
      self.bump_mutation_generation();
    }
    Ok(())
  }
}
