use crate::interaction::form_controls;
use crate::interaction::InteractionState;
use crate::text::caret::CaretAffinity;
use crate::tree::box_tree::{BoxNode, BoxTree, FormControl, FormControlKind, ReplacedType};
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
/// - Form-control caret/selection/IME preedit (including `appearance: none` controls).
pub(crate) fn apply_interaction_state_paint_overlays_to_fragment_tree(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  interaction_state: Option<&InteractionState>,
) {
  crate::interaction::document_selection::apply_document_selection_to_fragment_tree(
    box_tree,
    fragment_tree,
    interaction_state.and_then(|state| state.document_selection.as_ref()),
  );

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
      *value = form_controls::file_input_display_value(interaction_state, node_id);
      control.ime_preedit = None;
    }
    _ => {
      // Other control types (checkbox, select, button, etc.) do not have text editing state.
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
