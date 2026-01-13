use crate::dom2::{Document, NodeId, NodeKind};
use crate::text::caret::CaretAffinity;
use unicode_segmentation::UnicodeSegmentation;

use super::effective_disabled_dom2;
use super::state::{ImePreeditStateDom2, InteractionStateDom2, TextEditPaintStateDom2};
use super::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextEditStateDom2 {
  /// The focused `<input>`/`<textarea>`.
  node_id: NodeId,
  /// Insertion point in character indices (not bytes).
  caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  caret_affinity: CaretAffinity,
  /// Selection anchor for extending selections. When present and differs from `caret`, the control
  /// has an active selection.
  selection_anchor: Option<usize>,
}

impl TextEditStateDom2 {
  fn selection(&self) -> Option<(usize, usize)> {
    let anchor = self.selection_anchor?;
    if anchor == self.caret {
      return None;
    }
    Some(if anchor < self.caret {
      (anchor, self.caret)
    } else {
      (self.caret, anchor)
    })
  }
}

/// Interaction engine for `dom2::Document`-backed browsing mode.
///
/// This is a lightweight sibling of the legacy [`crate::interaction::InteractionEngine`] that
/// operates directly on the live `dom2` DOM tree.
///
/// Phase 1/2: tracking non-DOM-visible state (e.g. focus) and applying UA default actions.
///
/// Phase 3: text editing + IME for `<input>` / `<textarea>`.
#[derive(Debug, Default, Clone)]
pub struct InteractionEngineDom2 {
  state: InteractionStateDom2,
  text_edit: Option<TextEditStateDom2>,
}

impl InteractionEngineDom2 {
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  pub fn interaction_state(&self) -> &InteractionStateDom2 {
    &self.state
  }

  #[inline]
  pub fn focused(&self) -> Option<NodeId> {
    self.state.focused
  }

  /// Update the focused node id.
  ///
  /// Focus is stored only in the interaction state, not mirrored into DOM attributes.
  pub fn focus_node_id(
    &mut self,
    dom: &Document,
    node_id: Option<NodeId>,
    focus_visible: bool,
  ) -> bool {
    let mut changed = false;

    if self.state.focused != node_id {
      self.state.focused = node_id;
      // Clearing focus ends IME and text editing state.
      self.state.ime_preedit = None;
      self.text_edit = None;
      self.state.text_edit = None;
      self.state.focus_chain.clear();
      changed = true;
    }

    if self.state.focused.is_none() {
      if self.state.focus_visible {
        self.state.focus_visible = false;
        changed = true;
      }
    } else {
      if self.state.focus_visible != focus_visible {
        self.state.focus_visible = focus_visible;
        changed = true;
      }

      let focused = self.state.focused.unwrap();
      let next_chain = dom
        .ancestors(focused)
        .filter(|&id| matches!(dom.node(id).kind, NodeKind::Element { .. } | NodeKind::Slot { .. }))
        .collect::<Vec<_>>();
      if self.state.focus_chain != next_chain {
        self.state.focus_chain = next_chain;
        changed = true;
      }
    }

    changed
  }

  /// Apply the UA default action for a trusted primary click at `target`.
  ///
  /// This should be called only after the corresponding `"click"` event's default has **not**
  /// been prevented.
  pub fn activate_primary_click(&mut self, dom: &mut Document, target: NodeId) -> bool {
    // Determine the focusable click target first; this is used for focus updates and matches the
    // legacy behaviour where nested interactive content inside `<summary>` (e.g. `<a>`) takes focus.
    let click_focus_target = nearest_focusable_interactive_element(dom, target);

    let mut changed = false;

    // Update focus: if the click did not land on any focusable element, but it happened inside a
    // details summary, focus the summary (button-like).
    if let Some(focus_target) = click_focus_target {
      changed |= self.focus_node_id(dom, Some(focus_target), /* focus_visible */ false);
    } else if let Some((summary_id, _)) = nearest_details_summary(dom, target) {
      if !node_or_ancestor_is_inert_hidden_or_disabled(dom, summary_id) {
        changed |= self.focus_node_id(dom, Some(summary_id), /* focus_visible */ false);
      }
    }

    // Apply `<details>/<summary>` default toggle behaviour.
    if let Some((summary_id, details_id)) = nearest_details_summary(dom, target) {
      if !node_or_ancestor_is_inert_hidden_or_disabled(dom, summary_id) {
        changed |= toggle_details_open(dom, details_id);
      }
    }

    changed
  }

  fn ime_cancel_internal(&mut self) -> bool {
    let changed = self.state.ime_preedit.is_some();
    self.state.ime_preedit = None;
    changed
  }

  fn sync_text_edit_paint_state(&mut self) -> bool {
    let focused = self.state.focused;
    let next = match (focused, self.text_edit) {
      (Some(focused), Some(edit)) if edit.node_id == focused => Some(TextEditPaintStateDom2 {
        node_id: focused,
        caret: edit.caret,
        caret_affinity: edit.caret_affinity,
        selection: edit.selection(),
      }),
      _ => None,
    };

    if self.state.text_edit != next {
      self.state.text_edit = next;
      true
    } else {
      false
    }
  }

  /// Insert typed text into focused text control (`<input>` / `<textarea>`).
  pub fn text_input(&mut self, dom: &mut Document, text: &str) -> bool {
    let Some(focused) = self.state.focused else {
      return false;
    };

    if node_or_ancestor_is_inert_hidden_or_disabled(dom, focused) {
      return false;
    }

    let focused_is_text_input = is_text_input(dom, focused);
    let focused_is_textarea = is_textarea(dom, focused);
    if !(focused_is_text_input || focused_is_textarea) {
      return false;
    }
    if text.is_empty() {
      return false;
    }

    // Any direct text mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let current = if focused_is_textarea {
      dom.textarea_value(focused).ok().unwrap_or_default()
    } else {
      dom.input_value(focused).ok().unwrap_or("").to_string()
    };
    let current_len = current.chars().count();

    let mut edit = self.text_edit.unwrap_or(TextEditStateDom2 {
      node_id: focused,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
    });
    if edit.node_id != focused {
      edit = TextEditStateDom2 {
        node_id: focused,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let selection = edit.selection();
    let (replace_start, replace_end) = selection.unwrap_or((edit.caret, edit.caret));
    let start_byte = byte_offset_for_char_idx(&current, replace_start);
    let end_byte = byte_offset_for_char_idx(&current, replace_end);

    let mut next = String::with_capacity(
      current
        .len()
        .saturating_sub(end_byte.saturating_sub(start_byte))
        .saturating_add(text.len()),
    );
    next.push_str(&current[..start_byte]);
    next.push_str(text);
    next.push_str(&current[end_byte..]);

    if next != current {
      let setter_ok = if focused_is_textarea {
        dom.set_textarea_value(focused, &next).is_ok()
      } else {
        dom.set_input_value(focused, &next).is_ok()
      };
      changed |= setter_ok;
    }

    let next_len = next.chars().count();
    let inserted_chars = text.chars().count();
    let next_caret = replace_start.saturating_add(inserted_chars).min(next_len);

    self.text_edit = Some(TextEditStateDom2 {
      node_id: focused,
      caret: next_caret,
      // After inserting text, keep the caret attached to the inserted content.
      caret_affinity: CaretAffinity::Upstream,
      selection_anchor: None,
    });
    changed |= self.sync_text_edit_paint_state();
    changed
  }

  /// Update the active IME preedit (composition) string for the focused text control.
  pub fn ime_preedit(
    &mut self,
    dom: &mut Document,
    text: &str,
    cursor: Option<(usize, usize)>,
  ) -> bool {
    // Empty preedit text is treated as cancellation by most platform IMEs.
    if text.is_empty() {
      return self.ime_cancel(dom);
    }

    let Some(focused) = self.state.focused else {
      return self.ime_cancel(dom);
    };

    if node_or_ancestor_is_inert_hidden_or_disabled(dom, focused) {
      return self.ime_cancel(dom);
    }

    // Only text inputs and textareas participate in IME composition.
    if !(is_text_input(dom, focused) || is_textarea(dom, focused)) {
      return self.ime_cancel(dom);
    }

    match self.state.ime_preedit.as_mut() {
      Some(existing) if existing.node_id == focused => {
        if existing.text != text || existing.cursor != cursor {
          existing.text.clear();
          existing.text.push_str(text);
          existing.cursor = cursor;
          true
        } else {
          false
        }
      }
      _ => {
        self.state.ime_preedit = Some(ImePreeditStateDom2 {
          node_id: focused,
          text: text.to_string(),
          cursor,
        });
        true
      }
    }
  }

  /// Commit IME text into the focused text control, clearing any active preedit.
  pub fn ime_commit(&mut self, dom: &mut Document, text: &str) -> bool {
    let Some(_focused) = self.state.focused else {
      return self.ime_cancel(dom);
    };

    let mut changed = self.ime_cancel_internal();
    if text.is_empty() {
      return changed;
    }
    changed |= self.text_input(dom, text);
    changed
  }

  /// Cancel any active IME preedit string without mutating the DOM value.
  pub fn ime_cancel(&mut self, _dom: &mut Document) -> bool {
    self.ime_cancel_internal()
  }

  /// Handle key actions for focused text controls.
  pub fn key_action(&mut self, dom: &mut Document, key: KeyAction) -> bool {
    let Some(focused) = self.state.focused else {
      return false;
    };

    if node_or_ancestor_is_inert_hidden_or_disabled(dom, focused) {
      return false;
    }

    let focused_is_text_input = is_text_input(dom, focused);
    let focused_is_textarea = is_textarea(dom, focused);
    if !(focused_is_text_input || focused_is_textarea) {
      return false;
    }

    match key {
      KeyAction::Enter => {
        if focused_is_textarea {
          self.text_input(dom, "\n")
        } else {
          false
        }
      }
      KeyAction::SelectAll => self.select_all(dom, focused, focused_is_textarea),
      KeyAction::Backspace | KeyAction::Delete => {
        self.delete_for_key(dom, focused, key, focused_is_textarea)
      }
      KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
      | KeyAction::Home
      | KeyAction::End
      | KeyAction::ShiftHome
      | KeyAction::ShiftEnd => self.move_caret_for_key(dom, focused, key, focused_is_textarea),
      // Tab navigation, undo/redo, and other shortcuts are not implemented in this phase.
      _ => false,
    }
  }

  fn select_all(&mut self, dom: &Document, focused: NodeId, is_textarea: bool) -> bool {
    let current = if is_textarea {
      dom.textarea_value(focused).ok().unwrap_or_default()
    } else {
      dom.input_value(focused).ok().unwrap_or("").to_string()
    };
    let len = current.chars().count();

    self.text_edit = if len == 0 {
      Some(TextEditStateDom2 {
        node_id: focused,
        caret: 0,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
      })
    } else {
      Some(TextEditStateDom2 {
        node_id: focused,
        caret: len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: Some(0),
      })
    };

    self.sync_text_edit_paint_state()
  }

  fn delete_for_key(
    &mut self,
    dom: &mut Document,
    focused: NodeId,
    key: KeyAction,
    is_textarea: bool,
  ) -> bool {
    let current = if is_textarea {
      dom.textarea_value(focused).ok().unwrap_or_default()
    } else {
      dom.input_value(focused).ok().unwrap_or("").to_string()
    };
    let current_len = current.chars().count();

    let mut edit = self.text_edit.unwrap_or(TextEditStateDom2 {
      node_id: focused,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
    });
    if edit.node_id != focused {
      edit = TextEditStateDom2 {
        node_id: focused,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let selection = edit.selection();
    let Some((delete_start, delete_end, next_caret)) =
      text_delete_range_for_key(key, &current, edit.caret, selection)
    else {
      return false;
    };
    if delete_start >= delete_end {
      return false;
    }

    // Any direct text mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let start_byte = byte_offset_for_char_idx(&current, delete_start);
    let end_byte = byte_offset_for_char_idx(&current, delete_end);
    let mut next = String::with_capacity(current.len().saturating_sub(end_byte.saturating_sub(start_byte)));
    next.push_str(&current[..start_byte]);
    next.push_str(&current[end_byte..]);

    if next != current {
      let setter_ok = if is_textarea {
        dom.set_textarea_value(focused, &next).is_ok()
      } else {
        dom.set_input_value(focused, &next).is_ok()
      };
      changed |= setter_ok;
    }

    let next_len = next.chars().count();
    let next_caret = next_caret.min(next_len);
    self.text_edit = Some(TextEditStateDom2 {
      node_id: focused,
      caret: next_caret,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
    });
    changed |= self.sync_text_edit_paint_state();
    changed
  }

  fn move_caret_for_key(
    &mut self,
    dom: &Document,
    focused: NodeId,
    key: KeyAction,
    is_textarea: bool,
  ) -> bool {
    let current = if is_textarea {
      dom.textarea_value(focused).ok().unwrap_or_default()
    } else {
      dom.input_value(focused).ok().unwrap_or("").to_string()
    };
    let len = current.chars().count();

    let mut edit = self.text_edit.unwrap_or(TextEditStateDom2 {
      node_id: focused,
      caret: len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
    });
    if edit.node_id != focused {
      edit = TextEditStateDom2 {
        node_id: focused,
        caret: len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
      };
    }
    edit.caret = edit.caret.min(len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(len));

    let extend_selection = matches!(
      key,
      KeyAction::ShiftArrowLeft | KeyAction::ShiftArrowRight | KeyAction::ShiftHome | KeyAction::ShiftEnd
    );
    let move_left = matches!(key, KeyAction::ArrowLeft | KeyAction::ShiftArrowLeft);
    let move_right = matches!(key, KeyAction::ArrowRight | KeyAction::ShiftArrowRight);
    let to_home = matches!(key, KeyAction::Home | KeyAction::ShiftHome);
    let to_end = matches!(key, KeyAction::End | KeyAction::ShiftEnd);

    let original = edit;

    if !extend_selection {
      if let Some((start, end)) = edit.selection() {
        // Collapse selection without extending.
        let next_caret = if move_left || to_home {
          start
        } else if move_right || to_end {
          end
        } else {
          edit.caret
        };
        edit = TextEditStateDom2 {
          node_id: focused,
          caret: next_caret,
          caret_affinity: if move_right { CaretAffinity::Upstream } else { CaretAffinity::Downstream },
          selection_anchor: None,
        };
      } else {
        let next_caret = if to_home {
          0
        } else if to_end {
          len
        } else if move_left {
          prev_grapheme_boundary(&current, edit.caret).unwrap_or(edit.caret)
        } else if move_right {
          next_grapheme_boundary(&current, edit.caret).unwrap_or(edit.caret)
        } else {
          edit.caret
        };
        if next_caret != edit.caret {
          edit = TextEditStateDom2 {
            node_id: focused,
            caret: next_caret,
            caret_affinity: if move_right { CaretAffinity::Upstream } else { CaretAffinity::Downstream },
            selection_anchor: None,
          };
        }
      }
    } else {
      // Extend selection.
      let anchor = edit.selection_anchor.unwrap_or(edit.caret);
      let next_caret = if to_home {
        0
      } else if to_end {
        len
      } else if move_left {
        prev_grapheme_boundary(&current, edit.caret).unwrap_or(edit.caret)
      } else if move_right {
        next_grapheme_boundary(&current, edit.caret).unwrap_or(edit.caret)
      } else {
        edit.caret
      };
      edit = TextEditStateDom2 {
        node_id: focused,
        caret: next_caret,
        caret_affinity: if move_right || to_end { CaretAffinity::Upstream } else { CaretAffinity::Downstream },
        selection_anchor: Some(anchor),
      };
    }

    if edit == original {
      return false;
    }
    self.text_edit = Some(edit);
    self.sync_text_edit_paint_state()
  }
}

fn is_element_with_tag(dom: &Document, node_id: NodeId, tag: &str) -> bool {
  let node = dom.node(node_id);
  match &node.kind {
    NodeKind::Element { tag_name, .. } => tag_name.eq_ignore_ascii_case(tag),
    _ => false,
  }
}

fn is_summary(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "summary")
}

fn is_details(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "details")
}

/// Returns `Some(details_id)` if `summary` is the *details summary* for its parent `<details>`.
///
/// A details summary is:
/// - a `<summary>` element
/// - whose parent is a `<details>` element
/// - and which is the *first* `<summary>` element child of that `<details>`.
pub fn details_owner_for_summary(dom: &Document, summary: NodeId) -> Option<NodeId> {
  if !is_summary(dom, summary) {
    return None;
  }

  let details_id = dom.node(summary).parent?;
  if !is_details(dom, details_id) {
    return None;
  }

  // Find the first `<summary>` element child in DOM order (ignore nested summaries).
  let details = dom.node(details_id);
  for &child in &details.children {
    let child_node = dom.node(child);
    if child_node.parent != Some(details_id) {
      continue;
    }
    if is_summary(dom, child) {
      return (child == summary).then_some(details_id);
    }
  }

  None
}

/// Walk up the ancestor chain (including `start`) to find the nearest details summary.
///
/// Returns `(summary_id, details_id)` when found.
fn nearest_details_summary(dom: &Document, mut node_id: NodeId) -> Option<(NodeId, NodeId)> {
  loop {
    if let Some(details_id) = details_owner_for_summary(dom, node_id) {
      return Some((node_id, details_id));
    }
    node_id = dom.node(node_id).parent?;
  }
}

fn toggle_details_open(dom: &mut Document, details: NodeId) -> bool {
  if !is_details(dom, details) {
    return false;
  }
  let is_open = dom.has_attribute(details, "open").unwrap_or(false);
  dom
    .set_bool_attribute(details, "open", !is_open)
    .unwrap_or(false)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn node_or_ancestor_is_inert_hidden_or_disabled(dom: &Document, node_id: NodeId) -> bool {
  // Delegate to the shared dom2 implementation so we stay consistent with legacy interaction:
  // - `inert` / `<template>` contents behave as disconnected
  // - `hidden` is treated as not interactive
  // - `disabled` follows spec-correct fieldset rules for form controls, while still respecting a
  //   `disabled` attribute on the element itself (common for custom controls).
  effective_disabled_dom2::is_effectively_inert(node_id, dom)
    || effective_disabled_dom2::is_effectively_hidden(node_id, dom)
    || effective_disabled_dom2::is_effectively_disabled(node_id, dom)
}

fn parse_tabindex(dom: &Document, node_id: NodeId) -> Option<i32> {
  let raw = dom.get_attribute(node_id, "tabindex").ok().flatten()?;
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }
  raw.parse::<i32>().ok()
}

fn is_anchor_with_href(dom: &Document, node_id: NodeId) -> bool {
  let node = dom.node(node_id);
  let tag_name = match &node.kind {
    NodeKind::Element { tag_name, .. } => tag_name.as_str(),
    _ => return false,
  };
  if !(tag_name.eq_ignore_ascii_case("a") || tag_name.eq_ignore_ascii_case("area")) {
    return false;
  }
  let Some(href) = dom.get_attribute(node_id, "href").ok().flatten() else {
    return false;
  };
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return false;
  }
  // The browser UI doesn't execute JS, so `javascript:` URLs aren't meaningful navigation targets.
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return false;
  }
  true
}

fn is_input(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "input")
}

fn is_textarea(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "textarea")
}

fn is_select(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "select")
}

fn is_button(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "button")
}

fn input_type(dom: &Document, node_id: NodeId) -> &str {
  dom
    .get_attribute(node_id, "type")
    .ok()
    .flatten()
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn is_text_input(dom: &Document, node_id: NodeId) -> bool {
  if !is_input(dom, node_id) {
    return false;
  }

  let t = input_type(dom, node_id);
  // MVP heuristic: treat any non-button-ish, non-choice-ish input as a text control.
  !t.eq_ignore_ascii_case("checkbox")
    && !t.eq_ignore_ascii_case("radio")
    && !t.eq_ignore_ascii_case("button")
    && !t.eq_ignore_ascii_case("submit")
    && !t.eq_ignore_ascii_case("reset")
    && !t.eq_ignore_ascii_case("hidden")
    && !t.eq_ignore_ascii_case("range")
    && !t.eq_ignore_ascii_case("color")
    && !t.eq_ignore_ascii_case("file")
    && !t.eq_ignore_ascii_case("image")
}

/// MVP focusable predicate for pointer focus / blur decisions.
///
/// Mirrors the legacy `InteractionEngine` heuristic: native interactive elements + `tabindex`.
fn is_focusable_interactive_element(dom: &Document, node_id: NodeId) -> bool {
  if node_or_ancestor_is_inert_hidden_or_disabled(dom, node_id) {
    return false;
  }

  // HTML tabindex support: any parsed `tabindex` makes the element focusable via pointer and
  // programmatic focus, even when `tabindex < 0`.
  if parse_tabindex(dom, node_id).is_some() {
    // `input type=hidden` is never focusable, even if tabindex is set.
    if is_input(dom, node_id) && input_type(dom, node_id).eq_ignore_ascii_case("hidden") {
      return false;
    }
    return true;
  }

  if is_anchor_with_href(dom, node_id) {
    return true;
  }

  if details_owner_for_summary(dom, node_id).is_some() {
    return true;
  }

  if is_input(dom, node_id) {
    return !input_type(dom, node_id).eq_ignore_ascii_case("hidden");
  }

  is_textarea(dom, node_id) || is_select(dom, node_id) || is_button(dom, node_id)
}

fn nearest_focusable_interactive_element(dom: &Document, mut node_id: NodeId) -> Option<NodeId> {
  loop {
    if is_focusable_interactive_element(dom, node_id) {
      return Some(node_id);
    }
    node_id = dom.node(node_id).parent?;
  }
}

fn byte_offset_for_char_idx(text: &str, char_idx: usize) -> usize {
  if char_idx == 0 {
    return 0;
  }
  let mut count = 0usize;
  for (byte_idx, _) in text.char_indices() {
    if count == char_idx {
      return byte_idx;
    }
    count += 1;
  }
  text.len()
}

fn char_boundary_bytes(text: &str) -> Vec<usize> {
  let mut out = Vec::with_capacity(text.chars().count().saturating_add(1));
  for (byte_idx, _) in text.char_indices() {
    out.push(byte_idx);
  }
  out.push(text.len());
  out
}

fn char_idx_at_byte(boundary_bytes: &[usize], byte_idx: usize) -> usize {
  match boundary_bytes.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

fn grapheme_cluster_boundaries_char_idx(text: &str) -> Vec<usize> {
  if text.is_empty() {
    return vec![0];
  }
  let boundary_bytes = char_boundary_bytes(text);
  let mut out = Vec::with_capacity(boundary_bytes.len());
  for (byte_idx, _) in text.grapheme_indices(true) {
    out.push(char_idx_at_byte(&boundary_bytes, byte_idx));
  }
  out.push(boundary_bytes.len().saturating_sub(1));
  out
}

fn prev_grapheme_cluster(text: &str, caret: usize) -> Option<(usize, usize)> {
  if caret == 0 {
    return None;
  }
  let boundaries = grapheme_cluster_boundaries_char_idx(text);
  if boundaries.len() < 2 {
    return None;
  }
  let idx = boundaries
    .partition_point(|&b| b < caret)
    .saturating_sub(1)
    .min(boundaries.len().saturating_sub(2));
  Some((boundaries[idx], boundaries[idx + 1]))
}

fn next_grapheme_cluster(text: &str, caret: usize) -> Option<(usize, usize)> {
  let boundaries = grapheme_cluster_boundaries_char_idx(text);
  let len = *boundaries.last().unwrap_or(&0);
  if caret >= len || boundaries.len() < 2 {
    return None;
  }
  let idx = boundaries
    .partition_point(|&b| b <= caret)
    .saturating_sub(1)
    .min(boundaries.len().saturating_sub(2));
  Some((boundaries[idx], boundaries[idx + 1]))
}

fn prev_grapheme_boundary(text: &str, caret: usize) -> Option<usize> {
  prev_grapheme_cluster(text, caret).map(|(start, _)| start)
}

fn next_grapheme_boundary(text: &str, caret: usize) -> Option<usize> {
  next_grapheme_cluster(text, caret).map(|(_, end)| end)
}

fn text_delete_range_for_key(
  key: KeyAction,
  current: &str,
  caret: usize,
  selection: Option<(usize, usize)>,
) -> Option<(usize, usize, usize)> {
  if !matches!(key, KeyAction::Backspace | KeyAction::Delete) {
    return None;
  }
  let (start, end) = if let Some(selection) = selection {
    selection
  } else {
    match key {
      KeyAction::Backspace => prev_grapheme_cluster(current, caret)?,
      KeyAction::Delete => next_grapheme_cluster(current, caret)?,
      _ => return None,
    }
  };
  Some((start, end, start))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn clicking_first_summary_toggles_details_open() {
    let mut dom = crate::dom2::parse_html(
      "<!doctype html>\
       <details id=d>\
         <summary id=s1>One</summary>\
         <summary id=s2>Two</summary>\
         <div>Body</div>\
       </details>",
    )
    .unwrap();
    let details = dom.get_element_by_id("d").unwrap();
    let summary = dom.get_element_by_id("s1").unwrap();

    assert!(!dom.has_attribute(details, "open").unwrap());

    let mut engine = InteractionEngineDom2::new();
    assert!(engine.activate_primary_click(&mut dom, summary));

    assert!(dom.has_attribute(details, "open").unwrap());
  }

  #[test]
  fn clicking_non_first_summary_does_not_toggle_details() {
    let mut dom = crate::dom2::parse_html(
      "<!doctype html>\
       <details id=d>\
         <summary id=s1>One</summary>\
         <summary id=s2>Two</summary>\
         <div>Body</div>\
       </details>",
    )
    .unwrap();
    let details = dom.get_element_by_id("d").unwrap();
    let summary = dom.get_element_by_id("s2").unwrap();

    assert!(!dom.has_attribute(details, "open").unwrap());

    let mut engine = InteractionEngineDom2::new();
    // Non-first summaries are not the "details summary", so they should not toggle.
    assert!(!engine.activate_primary_click(&mut dom, summary));

    assert!(!dom.has_attribute(details, "open").unwrap());
  }

  fn find_element_node_id(dom: &Document, tag: &str) -> NodeId {
    let mut stack: Vec<NodeId> = vec![dom.root()];
    while let Some(id) = stack.pop() {
      let node = dom.node(id);
      if let NodeKind::Element { tag_name, .. } = &node.kind {
        if tag_name.eq_ignore_ascii_case(tag) {
          return id;
        }
      }
      for &child in node.children.iter().rev() {
        if dom.node(child).parent == Some(id) {
          stack.push(child);
        }
      }
    }
    panic!("missing element {tag}");
  }

  fn input_value(dom: &Document, node_id: NodeId) -> String {
    dom.input_value(node_id).unwrap_or("").to_string()
  }

  fn textarea_value(dom: &Document, node_id: NodeId) -> String {
    dom.textarea_value(node_id).unwrap_or_default()
  }

  fn set_text_selection_caret(engine: &mut InteractionEngineDom2, node_id: NodeId, caret: usize) {
    engine.text_edit = Some(TextEditStateDom2 {
      node_id,
      caret,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
    });
  }

  #[test]
  fn typing_inserts_into_input_and_textarea() {
    let mut dom = crate::dom2::parse_html(
      "<html><body><input value=\"hi\"><textarea>hi</textarea></body></html>",
    )
    .expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let textarea_id = find_element_node_id(&dom, "textarea");

    let mut engine = InteractionEngineDom2::new();

    engine.focus_node_id(&dom, Some(input_id), true);
    assert!(engine.text_input(&mut dom, "X"));
    assert_eq!(input_value(&dom, input_id), "hiX");

    engine.focus_node_id(&dom, Some(textarea_id), true);
    assert!(engine.text_input(&mut dom, "X"));
    assert_eq!(textarea_value(&dom, textarea_id), "hiX");
  }

  #[test]
  fn enter_inserts_newline_for_textarea_but_not_input() {
    let mut dom = crate::dom2::parse_html(
      "<html><body><textarea>hi</textarea><input value=\"hi\"></body></html>",
    )
    .expect("parse");
    let textarea_id = find_element_node_id(&dom, "textarea");
    let input_id = find_element_node_id(&dom, "input");

    let mut engine = InteractionEngineDom2::new();
    engine.focus_node_id(&dom, Some(textarea_id), true);
    assert!(engine.key_action(&mut dom, KeyAction::Enter));
    assert_eq!(textarea_value(&dom, textarea_id), "hi\n");

    engine.focus_node_id(&dom, Some(input_id), true);
    engine.key_action(&mut dom, KeyAction::Enter);
    assert_eq!(input_value(&dom, input_id), "hi");
  }

  #[test]
  fn ime_commit_inserts_at_non_end_caret() {
    let mut dom = crate::dom2::parse_html("<html><body><input value=\"abc\"></body></html>")
      .expect("parse");
    let input_id = find_element_node_id(&dom, "input");

    let mut engine = InteractionEngineDom2::new();
    engine.focus_node_id(&dom, Some(input_id), true);

    // Place caret between "a" and "b".
    set_text_selection_caret(&mut engine, input_id, 1);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    engine.ime_commit(&mut dom, "Z");

    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(input_value(&dom, input_id), "aZbc");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 2);
  }
}
