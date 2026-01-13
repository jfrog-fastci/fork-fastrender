use crate::interaction::state::FileSelection;
use crate::interaction::InteractionState;
use crate::text::caret::CaretAffinity;

/// Derive the display label/value for an `<input type="file">` control from the live interaction state.
///
/// This mirrors the logic in box generation so paint-time patching can reuse the same formatting:
/// - No selected files → `None`
/// - One selected file → show its path
/// - Multiple selected files → show a count (`"{n} files"`)
pub(crate) fn file_input_display_value(
  interaction_state: Option<&InteractionState>,
  node_id: usize,
) -> Option<String> {
  interaction_state
    .and_then(|state| state.form_state.files_for(node_id))
    .and_then(|files| file_input_display_value_from_files(files))
}

pub(crate) fn file_input_display_value_from_files(files: &[FileSelection]) -> Option<String> {
  if files.is_empty() {
    None
  } else if files.len() == 1 {
    Some(files[0].path.to_string_lossy().to_string())
  } else {
    Some(format!("{} files", files.len()))
  }
}

/// Normalizes a selection range in character indices for a text control.
///
/// - Clamps start/end into `[0, value_char_len]`
/// - Returns `None` for a collapsed selection
/// - Normalizes so `start < end`
pub(crate) fn normalize_text_selection(
  selection: Option<(usize, usize)>,
  value_char_len: usize,
) -> Option<(usize, usize)> {
  selection.and_then(|(start, end)| {
    let start = start.min(value_char_len);
    let end = end.min(value_char_len);
    if start == end {
      None
    } else if start < end {
      Some((start, end))
    } else {
      Some((end, start))
    }
  })
}

/// Derives caret + selection overlay state for a text control (`<input>` / `<textarea>`).
///
/// This is used during box generation, and also by paint-time patching code so interaction-only
/// updates (caret/selection moves, IME preedit) don't need to reimplement clamping and
/// normalization.
pub(crate) fn text_edit_state_for_value_char_len(
  interaction_state: Option<&InteractionState>,
  node_id: usize,
  value_char_len: usize,
) -> (usize, CaretAffinity, Option<(usize, usize)>) {
  let mut caret = value_char_len;
  let mut caret_affinity = CaretAffinity::Downstream;
  let mut selection: Option<(usize, usize)> = None;
  if let Some(edit) = interaction_state.and_then(|state| state.text_edit_for(node_id)) {
    caret = edit.caret.min(value_char_len);
    caret_affinity = edit.caret_affinity;
    selection = normalize_text_selection(edit.selection, value_char_len);
  }
  (caret, caret_affinity, selection)
}

/// Returns the current non-empty IME preedit string for `node_id`, when present.
pub(crate) fn ime_preedit_for_node(
  interaction_state: Option<&InteractionState>,
  node_id: usize,
) -> Option<String> {
  interaction_state
    .and_then(|state| state.ime_preedit_for(node_id))
    .filter(|t| !t.is_empty())
    .map(|t| t.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::interaction::state::TextEditPaintState;
  use std::path::PathBuf;

  #[test]
  fn file_input_display_value_formats_like_box_generation() {
    let node_id = 5usize;
    let mut state = InteractionState::default();

    // Missing entry.
    assert_eq!(file_input_display_value(Some(&state), node_id), None);

    // Empty entry.
    state.form_state.file_inputs.insert(node_id, Vec::new());
    assert_eq!(file_input_display_value(Some(&state), node_id), None);

    // Single file uses its path.
    state.form_state.file_inputs.insert(
      node_id,
      vec![FileSelection {
        path: PathBuf::from("/tmp/example.txt"),
        filename: "example.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: vec![1, 2, 3],
      }],
    );
    assert_eq!(
      file_input_display_value(Some(&state), node_id),
      Some("/tmp/example.txt".to_string())
    );

    // Multiple files uses a count.
    state.form_state.file_inputs.insert(
      node_id,
      vec![
        FileSelection {
          path: PathBuf::from("/tmp/a.txt"),
          filename: "a.txt".to_string(),
          content_type: "text/plain".to_string(),
          bytes: Vec::new(),
        },
        FileSelection {
          path: PathBuf::from("/tmp/b.txt"),
          filename: "b.txt".to_string(),
          content_type: "text/plain".to_string(),
          bytes: Vec::new(),
        },
      ],
    );
    assert_eq!(
      file_input_display_value(Some(&state), node_id),
      Some("2 files".to_string())
    );
  }

  #[test]
  fn text_edit_state_clamps_caret_and_normalizes_selection() {
    let node_id = 7usize;
    let value_char_len = 4usize;
    let mut state = InteractionState::default();
    state.text_edit = Some(TextEditPaintState {
      node_id,
      caret: 10,
      caret_affinity: CaretAffinity::Upstream,
      selection: Some((8, 2)),
    });

    let (caret, affinity, selection) =
      text_edit_state_for_value_char_len(Some(&state), node_id, value_char_len);
    assert_eq!(caret, 4);
    assert_eq!(affinity, CaretAffinity::Upstream);
    assert_eq!(selection, Some((2, 4)));

    // Mismatched node id -> defaults.
    let (caret, affinity, selection) =
      text_edit_state_for_value_char_len(Some(&state), node_id + 1, value_char_len);
    assert_eq!(caret, value_char_len);
    assert_eq!(affinity, CaretAffinity::Downstream);
    assert_eq!(selection, None);
  }
}

