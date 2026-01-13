use crate::interaction::form_controls;
use crate::interaction::InteractionState;
use crate::interaction::document_selection::DocumentSelectionIndex;
use crate::text::caret::CaretAffinity;
use crate::tree::box_tree::{
  BoxNode, BoxTree, FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem,
};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;

/// Apply paint-time interaction overlays to an already-laid-out fragment tree.
///
/// This is intended for cached paint paths (e.g. `BrowserDocument` repainting from a cached
/// `PreparedDocument`) where interaction state can change between paints without requiring a
/// cascade/layout rerun.
///
/// Overlays applied:
/// - Document selection highlights.
/// - Form-control paint state (caret/selection/IME preedit + live form-state overrides like
///   out-of-DOM values/checkedness/select selection).
pub(crate) fn apply_interaction_state_paint_overlays_to_fragment_tree(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  document_selection_index: &DocumentSelectionIndex,
  interaction_state: Option<&InteractionState>,
) {
  crate::interaction::document_selection::apply_document_selection_to_fragment_tree_with_index(
    fragment_tree,
    document_selection_index,
    interaction_state.and_then(|state| state.document_selection.as_ref()),
  );

  apply_form_control_paint_overlays_to_fragment_tree(box_tree, fragment_tree, interaction_state);
}

/// Apply paint-time form-control overlays (caret/selection/IME/file label) to an already-laid-out
/// fragment tree.
///
/// This intentionally does **not** touch document selection; callers that manage selection overlays
/// separately (e.g. `BrowserDocumentDom2` with `document_selection_dom2`) can use this helper without
/// clearing existing selection metadata.
pub(crate) fn apply_form_control_paint_overlays_to_fragment_tree(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  interaction_state: Option<&InteractionState>,
) {
  let box_id_to_styled_node_id = collect_box_id_to_styled_node_id(box_tree);

  apply_form_control_overlays_to_fragment_node(
    &mut fragment_tree.root,
    &box_id_to_styled_node_id,
    interaction_state,
  );
  for root in fragment_tree.additional_fragments.iter_mut() {
    apply_form_control_overlays_to_fragment_node(
      root,
      &box_id_to_styled_node_id,
      interaction_state,
    );
  }

  if let Some(existing) = fragment_tree.appearance_none_form_controls.as_ref() {
    if !existing.is_empty() {
      let mut updated: HashMap<usize, Arc<FormControl>> = HashMap::with_capacity(existing.len());
      for (box_id, control_arc) in existing.iter() {
        let mut control = (**control_arc).clone();
        if let Some(node_id) = box_id_to_styled_node_id.get(box_id).copied() {
          apply_form_control_paint_state(&mut control, node_id, interaction_state);
        } else {
          // Defensive: if we can't map the box id back to a DOM node id, clear paint-only state so
          // stale caret/selection/IME overlays don't persist across paints.
          clear_form_control_paint_state(&mut control);
        }
        updated.insert(*box_id, Arc::new(control));
      }
      fragment_tree.appearance_none_form_controls = Some(Arc::new(updated));
    }
  }
}

fn collect_box_id_to_styled_node_id(box_tree: &BoxTree) -> FxHashMap<usize, usize> {
  let mut mapping: FxHashMap<usize, usize> = FxHashMap::default();
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let Some(styled_id) = node.styled_node_id {
      mapping.insert(node.id, styled_id);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  mapping
}

fn select_placeholder_label_option_index(control: &SelectControl, required: bool) -> Option<usize> {
  if !required || control.multiple || control.size != 1 {
    return None;
  }

  // HTML: the placeholder label option exists when the first option in tree order has an empty
  // value attribute and is a direct child of `<select>` (i.e. not under `<optgroup>`).
  for (idx, item) in control.items.iter().enumerate() {
    match item {
      SelectItem::OptGroupLabel { .. } => continue,
      SelectItem::Option {
        value, in_optgroup, ..
      } => {
        if !*in_optgroup && value.is_empty() {
          return Some(idx);
        }
        return None;
      }
    }
  }

  None
}
fn apply_form_control_paint_state(
  control: &mut FormControl,
  node_id: usize,
  interaction_state: Option<&InteractionState>,
) {
  match &mut control.control {
    FormControlKind::Text {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let prev_empty = value.is_empty();
      let prev_invalid = control.invalid;
      if let Some(next_value) = interaction_state
        .and_then(|state| state.form_state().value_for(node_id))
        .filter(|v| *v != value.as_str())
      {
        *value = next_value.to_string();
      }
      if control.required {
        if value.is_empty() {
          control.invalid = true;
        } else if prev_invalid && prev_empty {
          // The previous snapshot was invalid with an empty value; assume requiredness and clear.
          control.invalid = false;
        }
      }
      let value_char_len = value.chars().count();
      let (next_caret, next_affinity, next_selection) =
        form_controls::text_edit_state_for_value_char_len(
          interaction_state,
          node_id,
          value_char_len,
        );
      *caret = next_caret;
      *caret_affinity = next_affinity;
      *selection = next_selection;

      control.ime_preedit = form_controls::ime_preedit_for_node(interaction_state, node_id);
    }
    FormControlKind::TextArea {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let prev_empty = value.is_empty();
      let prev_invalid = control.invalid;
      if let Some(next_value) = interaction_state
        .and_then(|state| state.form_state().value_for(node_id))
        .filter(|v| *v != value.as_str())
      {
        *value = next_value.to_string();
      }
      if control.required {
        if value.is_empty() {
          control.invalid = true;
        } else if prev_invalid && prev_empty {
          control.invalid = false;
        }
      }
      let value_char_len = value.chars().count();
      let (next_caret, next_affinity, next_selection) =
        form_controls::text_edit_state_for_value_char_len(
          interaction_state,
          node_id,
          value_char_len,
        );
      *caret = next_caret;
      *caret_affinity = next_affinity;
      *selection = next_selection;

      control.ime_preedit = form_controls::ime_preedit_for_node(interaction_state, node_id);
    }
    FormControlKind::File { value } => {
      let prev_empty = value.is_none();
      let prev_invalid = control.invalid;
      *value = form_controls::file_input_display_value(interaction_state, node_id);
      if control.required {
        if value.is_none() {
          control.invalid = true;
        } else if prev_invalid && prev_empty {
          control.invalid = false;
        }
      }
      control.ime_preedit = None;
    }
    FormControlKind::Checkbox {
      is_radio, checked, ..
    } => {
      let prev_checked = *checked;
      let prev_invalid = control.invalid;
      if let Some(next) =
        interaction_state.and_then(|state| state.form_state().checked_for(node_id))
      {
        *checked = next;
      }
      if control.required && !*is_radio {
        if !*checked {
          control.invalid = true;
        } else if prev_invalid && !prev_checked {
          control.invalid = false;
        }
      }
      control.ime_preedit = None;
    }
    FormControlKind::Select(select) => {
      if let Some(selected_set) =
        interaction_state.and_then(|state| state.form_state().select_selected_options(node_id))
      {
        let mut needs_update = false;
        for item in select.items.iter() {
          if let SelectItem::Option {
            node_id, selected, ..
          } = item
          {
            if selected_set.contains(node_id) != *selected {
              needs_update = true;
              break;
            }
          }
        }
        if !needs_update {
          control.ime_preedit = None;
          return;
        }

        let mut items = (*select.items).clone();
        let mut selected = Vec::new();
        for (idx, item) in items.iter_mut().enumerate() {
          if let SelectItem::Option {
            node_id,
            selected: item_selected,
            ..
          } = item
          {
            let is_selected = selected_set.contains(node_id);
            *item_selected = is_selected;
            if is_selected {
              selected.push(idx);
            }
          }
        }
        select.items = Arc::new(items);
        select.selected = selected;
      }
      // `select` constraint validation currently only cares about requiredness.
      if control.required {
        let invalid = if select.multiple || select.size != 1 {
          !select.selected.iter().any(|&idx| {
            matches!(
              select.items.get(idx),
              Some(SelectItem::Option {
                disabled: false,
                ..
              })
            )
          })
        } else {
          if select.selected.is_empty() {
            true
          } else if let Some(placeholder_idx) = select_placeholder_label_option_index(select, true)
          {
            select.selected.as_slice() == [placeholder_idx]
          } else {
            false
          }
        };
        control.invalid = invalid;
      }
      control.ime_preedit = None;
    }
    _ => {
      // Other control types (button, range, progress, etc.) do not have text editing state.
      control.ime_preedit = None;
    }
  }
}

fn clear_form_control_paint_state(control: &mut FormControl) {
  match &mut control.control {
    FormControlKind::Text {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let value_char_len = value.chars().count();
      *caret = value_char_len;
      *caret_affinity = CaretAffinity::Downstream;
      *selection = None;
      control.ime_preedit = None;
    }
    FormControlKind::TextArea {
      value,
      caret,
      caret_affinity,
      selection,
      ..
    } => {
      let value_char_len = value.chars().count();
      *caret = value_char_len;
      *caret_affinity = CaretAffinity::Downstream;
      *selection = None;
      control.ime_preedit = None;
    }
    FormControlKind::File { value } => {
      *value = None;
      control.ime_preedit = None;
    }
    _ => {
      control.ime_preedit = None;
    }
  }
}

fn apply_form_control_overlays_to_fragment_node(
  root: &mut FragmentNode,
  box_id_to_styled_node_id: &FxHashMap<usize, usize>,
  interaction_state: Option<&InteractionState>,
) {
  let mut stack: Vec<*mut FragmentNode> = vec![root as *mut _];
  while let Some(ptr) = stack.pop() {
    // SAFETY: We only push pointers to nodes owned by `root`, and we never mutate a `children`
    // vector while pointers into it are stored in `stack` (we use copy-on-write via
    // `children_mut()` and traverse each node once).
    let node = unsafe { &mut *ptr };

    if let FragmentContent::Replaced {
      replaced_type,
      box_id,
      ..
    } = &mut node.content
    {
      if let ReplacedType::FormControl(control) = replaced_type {
        if let Some(box_id) = *box_id {
          if let Some(node_id) = box_id_to_styled_node_id.get(&box_id).copied() {
            apply_form_control_paint_state(control, node_id, interaction_state);
          } else {
            clear_form_control_paint_state(control);
          }
        } else {
          clear_form_control_paint_state(control);
        }
      }
    }

    if matches!(
      node.content,
      FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
    ) {
      continue;
    }

    for child in node.children_mut().iter_mut().rev() {
      stack.push(child as *mut _);
    }
  }
}
