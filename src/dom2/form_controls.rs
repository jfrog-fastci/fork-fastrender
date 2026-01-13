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

#[inline]
fn attrs_remove_ci_mut(attrs: &mut Vec<Attribute>, name: &str) -> bool {
  let Some(idx) = attrs
    .iter()
    .position(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
  else {
    return false;
  };
  attrs.remove(idx);
  true
}

#[inline]
fn attrs_set_ci_mut(attrs: &mut Vec<Attribute>, name: &str, value: &str) -> bool {
  if let Some(existing) = attrs
    .iter_mut()
    .find(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
  {
    if existing.value == value {
      return false;
    }
    existing.value.clear();
    existing.value.push_str(value);
    true
  } else {
    attrs.push(Attribute::new_no_namespace(name, value));
    true
  }
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

#[inline]
fn is_input_type_file(type_attr: Option<&str>) -> bool {
  type_attr.is_some_and(|ty| trim_ascii_whitespace(ty).eq_ignore_ascii_case("file"))
}

fn input_default_value(type_attr: Option<&str>, value_attr: Option<&str>) -> String {
  // HTML: file inputs cannot be prefilled via markup.
  if is_input_type_file(type_attr) {
    return String::new();
  }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RadioGroupKey {
  root: NodeId,
  form_owner: Option<NodeId>,
  name: String,
}

impl Document {
  #[inline]
  fn is_file_input_node_kind(&self, kind: &NodeKind) -> bool {
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } = kind
    else {
      return false;
    };
    self.is_html_case_insensitive_namespace(namespace)
      && tag_name.eq_ignore_ascii_case("input")
      && is_input_type_file(attrs_get_ci(attributes, "type"))
  }

  fn is_html_form_element(&self, node: NodeId) -> bool {
    let Some(node) = self.nodes.get(node.index()) else {
      return false;
    };
    match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if self.is_html_case_insensitive_namespace(namespace)
        && tag_name.eq_ignore_ascii_case("form") =>
      {
        true
      }
      _ => false,
    }
  }

  fn input_radio_group_name(&self, input: NodeId) -> Option<&str> {
    let Some(node) = self.nodes.get(input.index()) else {
      return None;
    };
    let NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } = &node.kind
    else {
      return None;
    };
    if !self.is_html_case_insensitive_namespace(namespace)
      || !tag_name.eq_ignore_ascii_case("input")
    {
      return None;
    }

    let ty = attrs_get_ci(attributes, "type")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .unwrap_or("text");
    if !ty.eq_ignore_ascii_case("radio") {
      return None;
    }

    // Radio group semantics apply only when `name` is present and non-empty after ASCII trimming.
    let name = attrs_get_ci(attributes, "name")?;
    let name = trim_ascii_whitespace(name);
    (!name.is_empty()).then_some(name)
  }

  fn resolve_form_owner_for_radio_group(&self, control: NodeId, root: NodeId) -> Option<NodeId> {
    let node = self.nodes.get(control.index())?;
    let NodeKind::Element {
      namespace,
      attributes,
      ..
    } = &node.kind
    else {
      return None;
    };
    if !self.is_html_case_insensitive_namespace(namespace) {
      return None;
    }

    if let Some(form_attr) = attrs_get_ci(attributes, "form")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
    {
      let referenced = self.get_element_by_id_from(root, form_attr)?;
      return self.is_html_form_element(referenced).then_some(referenced);
    }

    for ancestor in self.ancestors(control).skip(1) {
      if self.is_html_form_element(ancestor) {
        return Some(ancestor);
      }
      if ancestor == root {
        break;
      }
    }

    None
  }

  fn radio_group_key(&self, input: NodeId) -> Option<RadioGroupKey> {
    let name = self.input_radio_group_name(input)?.to_string();
    let root = self.event_tree_root(input);
    let form_owner = self.resolve_form_owner_for_radio_group(input, root);
    Some(RadioGroupKey {
      root,
      form_owner,
      name,
    })
  }

  /// Enforce HTML radio-group mutual exclusivity for `input`.
  ///
  /// This is invoked after `input` becomes checked, and clears checkedness from other radios in the
  /// same group (tree root + form owner + name).
  ///
  /// Returns `true` iff any other radio's internal state was modified.
  fn uncheck_other_radios_in_group(
    &mut self,
    input: NodeId,
    set_dirty_checkedness_on_others: bool,
  ) -> bool {
    // Only enforce exclusivity for checked radios.
    let input_checked = self
      .input_states
      .get(input.index())
      .and_then(|s| s.as_ref())
      .is_some_and(|s| s.checkedness);
    if !input_checked {
      return false;
    }

    let Some(group) = self.radio_group_key(input) else {
      return false;
    };

    // Gather candidates first using immutable borrows so we can mutate in a second phase without
    // fighting borrow-checker aliasing.
    let mut to_uncheck: Vec<NodeId> = Vec::new();
    for idx in 0..self.nodes.len() {
      if idx == input.index() {
        continue;
      }

      let other = NodeId::from_index(idx);

      // Fast path: only consider currently checked inputs.
      let other_checked = self
        .input_states
        .get(idx)
        .and_then(|s| s.as_ref())
        .is_some_and(|s| s.checkedness);
      if !other_checked {
        continue;
      }

      if self.event_tree_root(other) != group.root {
        continue;
      }

      let Some(other_name) = self.input_radio_group_name(other) else {
        continue;
      };
      if other_name != group.name.as_str() {
        continue;
      }

      let other_owner = self.resolve_form_owner_for_radio_group(other, group.root);
      if other_owner != group.form_owner {
        continue;
      }

      to_uncheck.push(other);
    }

    let mut changed = false;
    for other in to_uncheck {
      let Some(state) = self
        .input_states
        .get_mut(other.index())
        .and_then(|s| s.as_mut())
      else {
        continue;
      };

      if state.checkedness {
        state.checkedness = false;
        if set_dirty_checkedness_on_others {
          state.dirty_checkedness = true;
        }
        self.record_form_state_mutation(other);
        changed = true;
      }
    }

    changed
  }

  pub(crate) fn init_form_control_state_for_node_kind(
    &self,
    kind: &NodeKind,
  ) -> (
    Option<InputState>,
    Option<TextareaState>,
    Option<OptionState>,
  ) {
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
      let is_file_input = is_input_type_file(type_attr);
      let value = if is_file_input {
        // File inputs cannot be prefilled via markup. We only populate their `.value` state from
        // the internal renderer snapshot attribute that represents a user selection.
        attrs_get_ci(attributes, "data-fastr-file-value")
          .unwrap_or("")
          .to_string()
      } else {
        let value_attr = attrs_get_ci(attributes, "value");
        input_default_value(type_attr, value_attr)
      };
      let dirty_value = is_file_input && !value.is_empty();
      let checkable = is_input_checkable(type_attr);
      let checkedness = checkable && attrs_has_ci(attributes, "checked");
      return (
        Some(InputState {
          value,
          dirty_value,
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
      let is_file = is_input_type_file(self.get_attribute(node, "type")?);

      // File inputs mirror their current "value string" (e.g. `C:\fakepath\...`) via a synthetic
      // internal attribute (`data-fastr-file-value`) used by the renderer pipeline.
      if name.eq_ignore_ascii_case("data-fastr-file-value") && is_file {
        let new_value = self
          .get_attribute(node, "data-fastr-file-value")?
          .unwrap_or("")
          .to_string();
        if let Some(state) = self
          .input_states
          .get_mut(node.index())
          .and_then(|s| s.as_mut())
        {
          let new_dirty = !new_value.is_empty();
          if state.value != new_value || state.dirty_value != new_dirty {
            state.value = new_value;
            state.dirty_value = new_dirty;
            self.record_form_state_mutation(node);
          }
        }
      }

      // HTML: when an input becomes type=file, its `.value` must be the empty string unless set via
      // a user file selection mechanism (out of scope here). Clear the current value regardless of
      // the dirty value flag so scripts cannot smuggle a non-empty value by changing `type`.
      if name.eq_ignore_ascii_case("type") && is_file {
        if let Some(state) = self
          .input_states
          .get_mut(node.index())
          .and_then(|s| s.as_mut())
        {
          state.value.clear();
          state.dirty_value = false;
        }
      } else if (name.eq_ignore_ascii_case("value") || name.eq_ignore_ascii_case("type")) && !dirty_value && !is_file {
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
      if (name.eq_ignore_ascii_case("checked") || name.eq_ignore_ascii_case("type"))
        && !dirty_checkedness
      {
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

      // Radio-group checkedness normalization:
      // When an input's checkedness is *not* dirty, mutations to attributes that affect radio-group
      // membership (`checked`/`type`/`name`/`form`) must still preserve the group invariant that at
      // most one radio is checked.
      if !dirty_checkedness
        && (name.eq_ignore_ascii_case("checked")
          || name.eq_ignore_ascii_case("type")
          || name.eq_ignore_ascii_case("name")
          || name.eq_ignore_ascii_case("form"))
      {
        let checked_now = self
          .input_states
          .get(node.index())
          .and_then(|s| s.as_ref())
          .is_some_and(|s| s.checkedness);
        if checked_now {
          // Attribute mutations are runtime actions; other radios that get unchecked become dirty so
          // their checkedness no longer tracks their `checked` content attribute.
          self.uncheck_other_radios_in_group(node, /* set_dirty_checkedness_on_others */ true);
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
    let node = self.node_checked(input)?;
    if self.is_file_input_node_kind(&node.kind) {
      let state_value = self.input_state(input)?.value.as_str();
      if !state_value.is_empty() {
        return Ok(state_value);
      }
      // For file inputs, the browser-like "value string" is represented by the internal renderer
      // attribute `data-fastr-file-value`. Prefer the internal state slot, but fall back to the
      // attribute so imported renderer snapshots remain observable.
      if let Ok(Some(attr_value)) = self.get_attribute(input, "data-fastr-file-value") {
        return Ok(attr_value);
      }
      return Ok("");
    }
    Ok(self.input_state(input)?.value.as_str())
  }

  pub fn set_input_value(&mut self, input: NodeId, value: &str) -> Result<bool, DomError> {
    let _ = self.node_checked(input)?;
    // Validate node type (must be an HTML <input> with internal state).
    let _ = self.input_state(input)?;

    // HTML: `<input type=file>.value` is only script-settable to the empty string (to clear an
    // existing selection). Setting any other value is forbidden for security reasons.
    if is_input_type_file(self.get_attribute(input, "type")?) {
      if !value.is_empty() {
        // No-op: do not mutate state and do not bump mutation generation.
        return Ok(false);
      }
      // Clearing is allowed.
      return self.set_file_input_value_string(input, "");
    }

    let state = self.input_state_mut(input)?;
    if state.value == value {
      return Ok(false);
    }
    state.dirty_value = true;
    state.value.clear();
    state.value.push_str(value);
    self.record_form_state_mutation(input);
    self.bump_mutation_generation_classified();
    Ok(true)
  }

  /// Host-only API: set the browser-like "value string" for an `<input type="file">`.
  ///
  /// The renderer pipeline uses an internal `data-fastr-file-value` attribute to represent the
  /// pseudo value string (`C:\fakepath\...`) for validation and painting. Unlike `set_input_value`,
  /// this method allows setting a non-empty value string (mirroring a trusted file picker / drop).
  pub fn set_file_input_value_string(
    &mut self,
    input: NodeId,
    value_string: &str,
  ) -> Result<bool, DomError> {
    let is_file_input = {
      let node = self.node_checked(input)?;
      self.is_file_input_node_kind(&node.kind)
    };
    if !is_file_input {
      return Err(DomError::InvalidNodeTypeError);
    }

    let mut changed = false;
    {
      let state = self.input_state_mut(input)?;
      if state.value != value_string {
        state.value.clear();
        state.value.push_str(value_string);
        changed = true;
      }
      let new_dirty = !value_string.is_empty();
      if state.dirty_value != new_dirty {
        state.dirty_value = new_dirty;
        changed = true;
      }
    }

    {
      let node = self.node_checked_mut(input)?;
      let NodeKind::Element { attributes, .. } = &mut node.kind else {
        return Err(DomError::InvalidNodeTypeError);
      };
      let attr_changed = if value_string.is_empty() {
        attrs_remove_ci_mut(attributes, "data-fastr-file-value")
      } else {
        attrs_set_ci_mut(attributes, "data-fastr-file-value", value_string)
      };
      changed |= attr_changed;
    }

    if changed {
      self.record_form_state_mutation(input);
      self.bump_mutation_generation_classified();
    }
    Ok(changed)
  }

  pub fn input_checked(&self, input: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(input)?;
    Ok(self.input_state(input)?.checkedness)
  }

  pub fn set_input_checked(&mut self, input: NodeId, checked: bool) -> Result<bool, DomError> {
    let _ = self.node_checked(input)?;
    let state = self.input_state_mut(input)?;
    if state.checkedness == checked {
      // Even if the target checkedness is unchanged, the call may still normalize radio group
      // exclusivity by unchecking other radios. Only treat it as a no-op if no state changes.
      if checked && self.uncheck_other_radios_in_group(input, /* set_dirty_checkedness_on_others */ true)
      {
        self.bump_mutation_generation_classified();
        return Ok(true);
      }
      return Ok(false);
    }
    state.dirty_checkedness = true;
    state.checkedness = checked;
    if checked {
      self.uncheck_other_radios_in_group(input, /* set_dirty_checkedness_on_others */ true);
    }
    self.record_form_state_mutation(input);
    self.bump_mutation_generation_classified();
    Ok(true)
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

  pub fn set_textarea_value(&mut self, textarea: NodeId, value: &str) -> Result<bool, DomError> {
    let _ = self.node_checked(textarea)?;

    // `TextareaState.value` is only authoritative once the dirty flag is set. While not dirty, the
    // effective value is derived from descendant text nodes.
    let no_change = {
      let state = self.textarea_state(textarea)?;
      if state.dirty_value {
        state.value == value
      } else {
        self.textarea_default_value(textarea)? == value
      }
    };
    if no_change {
      return Ok(false);
    }

    let state = self.textarea_state_mut(textarea)?;
    state.dirty_value = true;
    state.value.clear();
    state.value.push_str(value);
    self.record_form_state_mutation(textarea);
    self.bump_mutation_generation_classified();
    Ok(true)
  }

  pub fn option_selected(&self, option: NodeId) -> Result<bool, DomError> {
    let _ = self.node_checked(option)?;
    Ok(self.option_state(option)?.selectedness)
  }

  pub fn set_option_selected(&mut self, option: NodeId, selected: bool) -> Result<bool, DomError> {
    let _ = self.node_checked(option)?;
    let select_owner = self.ancestors(option).skip(1).find(|&ancestor| {
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

    let current_selectedness = self.option_state(option)?.selectedness;
    let mut changed = false;

    // Avoid setting dirty flags / bumping generation when the effective selection state is
    // unchanged.
    if current_selectedness != selected {
      let state = self.option_state_mut(option)?;
      state.dirty_selectedness = true;
      state.selectedness = selected;
      changed = true;
    }

    let Some(select) = select_owner else {
      if changed {
        self.record_form_state_mutation(repaint_target);
        self.bump_mutation_generation_classified();
      }
      return Ok(changed);
    };

    let multiple = self.has_attribute(select, "multiple")?;
    if multiple {
      if changed {
        self.record_form_state_mutation(repaint_target);
        self.bump_mutation_generation_classified();
      }
      return Ok(changed);
    }

    // Single-select: ensure mutual exclusion when an option is selected.
    if selected {
      let options = self.select_options(select);
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
          changed = true;
        }
      }
    }

    if changed {
      self.record_form_state_mutation(repaint_target);
      self.bump_mutation_generation_classified();
    }
    // Unlike `selectedIndex`, the `option.selected = false` IDL setter is allowed to leave a
    // single-select with no selected option. `select_selected_index()` / `select_value()` shims
    // normalize this state on read when needed.
    Ok(changed)
  }

  pub fn reset_input(&mut self, input: NodeId) -> Result<(), DomError> {
    let _ = self.node_checked(input)?;
    let type_attr = self.get_attribute(input, "type")?;
    let value_attr = self.get_attribute(input, "value")?;
    let default_value = input_default_value(type_attr, value_attr);
    let is_file_input = is_input_type_file(type_attr);
    let checkable = is_input_checkable(type_attr);
    let checked_attr = self.has_attribute(input, "checked")?;
    let default_checkedness = checkable && checked_attr;
    {
      let state = self.input_state_mut(input)?;
      state.dirty_value = false;
      state.value = default_value;
      state.dirty_checkedness = false;
      state.checkedness = default_checkedness;
    }
    if is_file_input {
      // File inputs must reset any internal file selection value string.
      if let Ok(node) = self.node_checked_mut(input) {
        if let NodeKind::Element { attributes, .. } = &mut node.kind {
          attrs_remove_ci_mut(attributes, "data-fastr-file-value");
        }
      }
    }
    if default_checkedness {
      // Reset operations restore default checkedness and clear dirty flags, so other radios that get
      // unchecked must not become dirty.
      self.uncheck_other_radios_in_group(input, /* set_dirty_checkedness_on_others */ false);
    }
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
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if self.is_html_case_insensitive_namespace(namespace)
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
        if self
          .nodes
          .get(child.index())
          .is_some_and(|child_node| child_node.parent == Some(node_id))
        {
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
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if self.is_html_case_insensitive_namespace(namespace)
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
        let is_file_input = is_input_type_file(type_attr);
        let checkable = is_input_checkable(type_attr);
        let checked_attr = self.has_attribute(node_id, "checked")?;
        let default_checkedness = checkable && checked_attr;
        {
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
        if is_file_input {
          if let Some(node) = self.nodes.get_mut(node_id.index()) {
            if let NodeKind::Element { attributes, .. } = &mut node.kind {
              attrs_remove_ci_mut(attributes, "data-fastr-file-value");
            }
          }
        }
        if default_checkedness {
          // Form reset clears dirty flags, so other radios that get unchecked should stay not-dirty.
          let changed = self.uncheck_other_radios_in_group(
            node_id, /* set_dirty_checkedness_on_others */ false,
          );
          any |= changed;
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
        if self
          .nodes
          .get(child.index())
          .is_some_and(|child_node| child_node.parent == Some(node_id))
        {
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
      //
      // NOTE: avoid holding borrows into `node.node_type` across attribute mutations below.
      let (is_input, is_textarea, is_option, in_html_namespace) = match &node.node_type {
        DomNodeType::Element { tag_name, namespace, .. } => (
          tag_name.eq_ignore_ascii_case("input"),
          tag_name.eq_ignore_ascii_case("textarea"),
          tag_name.eq_ignore_ascii_case("option"),
          namespace.is_empty() || namespace == HTML_NAMESPACE,
        ),
        _ => {
          let len = node.children.len();
          let children_ptr = node.children.as_mut_ptr();
          for idx in (0..len).rev() {
            stack.push(unsafe { children_ptr.add(idx) });
          }
          continue;
        }
      };
      if !in_html_namespace {
        let len = node.children.len();
        let children_ptr = node.children.as_mut_ptr();
        for idx in (0..len).rev() {
          stack.push(unsafe { children_ptr.add(idx) });
        }
        continue;
      }

      if is_input {
        // Avoid holding a borrowed `&str` from the node's attribute map across mutations (calling
        // `set_attribute`/`toggle_bool_attribute` can reallocate).
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
            // Preserve authored `value=` attribute presence semantics:
            // - Avoid injecting `value=""` when the input has no value attribute and the current
            //   value is empty (the renderer treats missing as empty anyway).
            // - Avoid injecting `value="on"` for checkbox/radio inputs when the value attribute is
            //   missing and the value hasn't been dirtied: "on" is the default IDL value, not an
            //   authored attribute.
            let has_value_attr = node.get_attribute_ref("value").is_some();
            let is_default_checkable_value_without_attr =
              is_checkable && !has_value_attr && !state.dirty_value && state.value == "on";
            let should_set_value_attr = !is_default_checkable_value_without_attr
              && (has_value_attr || !state.value.is_empty() || state.dirty_value);
            if should_set_value_attr {
              node.set_attribute("value", &state.value);
            } else {
              node.remove_attribute("value");
            }
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
      } else if is_textarea {
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
      } else if is_option {
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
