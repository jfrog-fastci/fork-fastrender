use super::{Attribute, Document, DomError, NodeId, NodeKind, RendererDomMapping, NULL_NAMESPACE};
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};

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
fn attrs_get_ci<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
  attrs
    .iter()
    .find(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
    .map(|attr| attr.value.as_str())
}

#[inline]
fn attrs_has_ci(attrs: &[Attribute], name: &str) -> bool {
  attrs
    .iter()
    .any(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
}

fn is_input_checkable(type_attr: Option<&str>) -> bool {
  let ty = type_attr.unwrap_or("text");
  ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio")
}

#[inline]
fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute parsing ignores leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but does
  // not treat all Unicode whitespace as ignorable (e.g. NBSP).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn input_default_value(type_attr: Option<&str>, value_attr: Option<&str>) -> String {
  if let Some(value) = value_attr {
    return value.to_string();
  }
  // HTML: checkbox/radio inputs default to value "on" when the `value` content attribute is
  // missing.
  //
  // This matches browser-observable `HTMLInputElement.value` defaults and ensures form submission
  // uses the correct value when no explicit `value` attribute is present.
  if is_input_checkable(type_attr) {
    return "on".to_string();
  }
  String::new()
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
      let type_attr = attrs_get_ci(attributes, "type");
      let value_attr = attrs_get_ci(attributes, "value");
      let value = input_default_value(type_attr, value_attr);
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
      .ok_or(DomError::InvalidNodeTypeError)
  }

  fn input_state_mut(&mut self, node: NodeId) -> Result<&mut InputState, DomError> {
    self
      .input_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeTypeError)
  }

  fn textarea_state(&self, node: NodeId) -> Result<&TextareaState, DomError> {
    self
      .textarea_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .ok_or(DomError::InvalidNodeTypeError)
  }

  fn textarea_state_mut(&mut self, node: NodeId) -> Result<&mut TextareaState, DomError> {
    self
      .textarea_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeTypeError)
  }

  fn option_state(&self, node: NodeId) -> Result<&OptionState, DomError> {
    self
      .option_states
      .get(node.index())
      .and_then(|s| s.as_ref())
      .ok_or(DomError::InvalidNodeTypeError)
  }

  fn option_state_mut(&mut self, node: NodeId) -> Result<&mut OptionState, DomError> {
    self
      .option_states
      .get_mut(node.index())
      .and_then(|s| s.as_mut())
      .ok_or(DomError::InvalidNodeTypeError)
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
      if (name.eq_ignore_ascii_case("value") || name.eq_ignore_ascii_case("type")) && !dirty_value {
        let type_attr = self.get_attribute(node, "type")?;
        let value_attr = self.get_attribute(node, "value")?;
        let new_value = input_default_value(type_attr, value_attr);
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
    self.record_form_state_mutation(input);
    self.bump_mutation_generation_classified();
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
    self.record_form_state_mutation(input);
    self.bump_mutation_generation_classified();
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

  pub(crate) fn textarea_value_is_dirty(&self, textarea: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(textarea)?;
    Ok(self.textarea_state(textarea)?.dirty_value)
  }

  pub fn set_textarea_value(&mut self, textarea: NodeId, value: &str) -> Result<(), DomError> {
    let _ = self.node_checked(textarea)?;
    let state = self.textarea_state_mut(textarea)?;
    state.dirty_value = true;
    state.value.clear();
    state.value.push_str(value);
    self.record_form_state_mutation(textarea);
    self.bump_mutation_generation_classified();
    Ok(())
  }

  pub fn option_selected(&self, option: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(option)?;
    Ok(self.option_state(option)?.selectedness)
  }

  pub fn set_option_selected(&mut self, option: NodeId, selected: bool) -> Result<(), DomError> {
    let _ = self.node_checked(option)?;
    let select_owner = self
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
      });
    let repaint_target = select_owner.unwrap_or(option);

    {
      let state = self.option_state_mut(option)?;
      state.dirty_selectedness = true;
      state.selectedness = selected;
    }
    self.record_form_state_mutation(repaint_target);
    self.bump_mutation_generation_classified();

    let Some(select) = select_owner else {
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
          self.record_form_state_mutation(repaint_target);
          self.bump_mutation_generation_classified();
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
      self.record_form_state_mutation(repaint_target);
      self.bump_mutation_generation_classified();
    }
    Ok(())
  }

  pub fn reset_input(&mut self, input: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(input)?;
    let type_attr = self.get_attribute(input, "type")?;
    let value_attr = self.get_attribute(input, "value")?;
    let default_value = input_default_value(type_attr, value_attr);
    let checkable = is_input_checkable(type_attr);
    let checked_attr = self.has_attribute(input, "checked")?;
    let default_checkedness = checkable && checked_attr;
    let state = self.input_state_mut(input)?;
    state.dirty_value = false;
    state.value = default_value;
    state.dirty_checkedness = false;
    state.checkedness = default_checkedness;
    self.record_form_state_mutation(input);
    self.bump_mutation_generation_classified();
    Ok(())
  }

  pub fn reset_textarea(&mut self, textarea: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(textarea)?;
    let state = self.textarea_state_mut(textarea)?;
    state.dirty_value = false;
    state.value.clear();
    self.record_form_state_mutation(textarea);
    self.bump_mutation_generation_classified();
    Ok(())
  }

  pub fn reset_option(&mut self, option: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(option)?;
    let selectedness = self.has_attribute(option, "selected")?;
    let state = self.option_state_mut(option)?;
    state.dirty_selectedness = false;
    state.selectedness = selectedness;
    self.record_form_state_mutation(option);
    self.bump_mutation_generation_classified();
    Ok(())
  }

  fn textarea_default_value(&self, textarea: NodeId) -> Result<String, DomError> {
    let node = self.node_checked(textarea)?;
    match &node.kind {
      NodeKind::Element { tag_name, namespace, .. }
        if self.is_html_case_insensitive_namespace(namespace)
          && tag_name.eq_ignore_ascii_case("textarea") => {}
      _ => return Err(DomError::InvalidNodeTypeError),
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
      _ => return Err(DomError::InvalidNodeTypeError),
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
        let type_attr = self.get_attribute(node_id, "type")?;
        let value_attr = self.get_attribute(node_id, "value")?;
        let default_value = input_default_value(type_attr, value_attr);
        let checkable = is_input_checkable(type_attr);
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
          self.record_form_state_mutation(node_id);
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
          self.record_form_state_mutation(node_id);
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
          self.record_form_state_mutation(node_id);
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
      self.bump_mutation_generation_classified();
    }
    Ok(())
  }

  /// Project runtime form control state (e.g. `.value`, `.checked`) into a renderer DOM snapshot.
  ///
  /// `dom2` tracks mutable form control state out-of-markup using internal slots (`InputState`,
  /// `TextareaState`, `OptionState`). When rendering via the legacy (dom1) pipeline, the renderer
  /// consumes an immutable [`DomNode`] snapshot that only includes content attributes.
  ///
  /// This helper mutates a renderer snapshot in-place so paints reflect live runtime state:
  /// - `<input>`: mirror current value into `value=` (except `type=file`) and checkedness into
  ///   `checked` for checkbox/radio inputs.
  /// - `<textarea>`: mirror edited values into `data-fastr-value` (and remove it when not dirty).
  /// - `<option>`: mirror selectedness into `selected`.
  pub(crate) fn project_form_control_state_into_renderer_dom_snapshot(
    &self,
    snapshot_dom: &mut DomNode,
    mapping: &RendererDomMapping,
  ) {
    // Avoid recursion for deep DOM trees and to keep preorder ids aligned with
    // `crate::dom::enumerate_dom_ids`.
    let mut stack: Vec<*mut DomNode> = vec![snapshot_dom as *mut DomNode];
    let mut preorder_id: usize = 0;

    while let Some(ptr) = stack.pop() {
      preorder_id += 1;

      // Safety: pointers are derived from a live `DomNode` tree; we never mutate a node's `children`
      // vector while pointers into it are stored on the stack.
      let node = unsafe { &mut *ptr };

      let Some(dom2_id) = mapping.node_id_for_preorder(preorder_id) else {
        // Mapping should cover the entire snapshot, but be defensive.
        let len = node.children.len();
        let children_ptr = node.children.as_mut_ptr();
        for idx in (0..len).rev() {
          stack.push(unsafe { children_ptr.add(idx) });
        }
        continue;
      };

      // Apply only to HTML elements in the HTML namespace.
      let (tag_name, namespace) = match &node.node_type {
        DomNodeType::Element { tag_name, namespace, .. } => (tag_name.as_str(), namespace.as_str()),
        _ => {
          let len = node.children.len();
          let children_ptr = node.children.as_mut_ptr();
          for idx in (0..len).rev() {
            stack.push(unsafe { children_ptr.add(idx) });
          }
          continue;
        }
      };
      if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
        let len = node.children.len();
        let children_ptr = node.children.as_mut_ptr();
        for idx in (0..len).rev() {
          stack.push(unsafe { children_ptr.add(idx) });
        }
        continue;
      }

      if tag_name.eq_ignore_ascii_case("input") {
        let (is_file, is_checkable) = {
          let input_type = node
            .get_attribute_ref("type")
            .map(trim_ascii_whitespace)
            .unwrap_or("text");
          (
            input_type.eq_ignore_ascii_case("file"),
            is_input_checkable(Some(input_type)),
          )
        };

        if !is_file {
          if let Some(state) = self
            .input_states
            .get(dom2_id.index())
            .and_then(|s| s.as_ref())
          {
            node.set_attribute("value", &state.value);
          }
        }

        if is_checkable {
          if let Some(state) = self
            .input_states
            .get(dom2_id.index())
            .and_then(|s| s.as_ref())
          {
            node.toggle_bool_attribute("checked", state.checkedness);
          }
        }
      } else if tag_name.eq_ignore_ascii_case("textarea") {
        if let Some(state) = self
          .textarea_states
          .get(dom2_id.index())
          .and_then(|s| s.as_ref())
        {
          if state.dirty_value {
            node.set_attribute("data-fastr-value", &state.value);
          } else {
            // For non-dirty textareas, the current value is derived from descendant text nodes with
            // HTML-specific normalization (see `crate::dom::textarea_value`); keep `data-fastr-value`
            // absent so painting follows the default semantics (and doesn't preserve an otherwise
            // stripped leading newline).
            node.remove_attribute("data-fastr-value");
          }
        }
      } else if tag_name.eq_ignore_ascii_case("option") {
        if let Some(state) = self
          .option_states
          .get(dom2_id.index())
          .and_then(|s| s.as_ref())
        {
          node.toggle_bool_attribute("selected", state.selectedness);
        }
      }

      let len = node.children.len();
      let children_ptr = node.children.as_mut_ptr();
      for idx in (0..len).rev() {
        // SAFETY: `children_ptr` came from `node.children` and the vector is not mutated until after
        // this node is processed, so these pointers remain valid.
        stack.push(unsafe { children_ptr.add(idx) });
      }
    }
  }
}
