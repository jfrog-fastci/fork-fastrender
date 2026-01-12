use crate::geometry::Rect;
use crate::interaction::KeyAction;
use crate::tree::box_tree::{SelectControl, SelectItem};

#[path = "select_dropdown/choice.rs"]
mod choice;

pub use choice::SelectDropdownChoice;

/// Compute the next selectable `<option>` row index for a dropdown `<select>` popup.
///
/// Returns the **item index** into [`SelectControl::items`] for the option that should become
/// selected after applying the given key action.
///
/// Notes:
/// - This operates on the painter's flattened [`SelectControl`] snapshot (so it naturally skips
///   `hidden`/`display:none` options that were not painted).
/// - Only arrow / Home / End keys are handled; other keys return `None`.
pub fn next_enabled_option_item_index(control: &SelectControl, key: KeyAction) -> Option<usize> {
  use KeyAction::{ArrowDown, ArrowUp, End, Home};

  if !matches!(key, ArrowUp | ArrowDown | Home | End) {
    return None;
  }

  let options = control
    .items
    .iter()
    .enumerate()
    .filter_map(|(idx, item)| match item {
      SelectItem::Option { disabled, .. } => Some((idx, *disabled)),
      _ => None,
    })
    .collect::<Vec<_>>();

  if options.is_empty() {
    return None;
  }

  let selected_pos = control
    .selected
    .last()
    .copied()
    .and_then(|selected_item_idx| {
      options
        .iter()
        .position(|(idx, _)| *idx == selected_item_idx)
    });

  let mut first_enabled: Option<usize> = None;
  let mut last_enabled: Option<usize> = None;
  for (pos, (_, disabled)) in options.iter().enumerate() {
    if !*disabled {
      if first_enabled.is_none() {
        first_enabled = Some(pos);
      }
      last_enabled = Some(pos);
    }
  }

  let first_enabled = first_enabled?;
  let last_enabled = last_enabled.unwrap_or(first_enabled);
  let anchor = selected_pos.unwrap_or(first_enabled);

  let next = match key {
    ArrowDown => {
      let mut found = None;
      for pos in (anchor + 1)..options.len() {
        if !options[pos].1 {
          found = Some(pos);
          break;
        }
      }
      found.unwrap_or(last_enabled)
    }
    ArrowUp => {
      let mut found = None;
      for pos in (0..anchor).rev() {
        if !options[pos].1 {
          found = Some(pos);
          break;
        }
      }
      found.unwrap_or(first_enabled)
    }
    Home => first_enabled,
    End => last_enabled,
    _ => return None,
  };

  // If we clamped and the anchor was already selected, treat as a no-op.
  if next == anchor && selected_pos.is_some() {
    return Some(options[anchor].0);
  }

  Some(options[next].0)
}

/// Returns the currently-selected `<option>` as a [`SelectDropdownChoice`].
///
/// This is primarily intended for keyboard UX: when a dropdown popup is open and the user presses
/// Enter/Space, the UI typically "accepts" the current selection and closes the popup.
///
/// If the selected item refers to a disabled option, this returns `None` (a disabled option is not
/// user-selectable).
pub fn selected_choice(
  select_node_id: usize,
  control: &SelectControl,
) -> Option<SelectDropdownChoice> {
  for &item_index in control.selected.iter().rev() {
    let Some(item) = control.items.get(item_index) else {
      continue;
    };
    match item {
      SelectItem::OptGroupLabel { .. } => {}
      SelectItem::Option {
        disabled, node_id, ..
      } => {
        if !*disabled {
          return Some(SelectDropdownChoice::new(select_node_id, *node_id));
        }
      }
    }
  }
  None
}

#[derive(Debug, Clone)]
struct OpenSelectDropdown {
  select_node_id: usize,
  control: SelectControl,
  #[cfg_attr(not(feature = "browser_ui"), allow(dead_code))]
  anchor: Option<Rect>,
}

#[derive(Debug, Default, Clone)]
pub struct SelectDropdown {
  open: Option<OpenSelectDropdown>,
  /// Last popup rectangle (in egui points when `SelectDropdown::ui` is used).
  ///
  /// This is primarily used by UIs to suppress page input while interacting with the dropdown.
  last_popup_rect: Option<Rect>,
}

impl SelectDropdown {
  pub fn open(&mut self, select_node_id: usize, control: SelectControl, anchor: Option<Rect>) {
    self.open = Some(OpenSelectDropdown {
      select_node_id,
      control,
      anchor,
    });
    self.last_popup_rect = None;
  }

  pub fn close(&mut self) {
    self.open = None;
    self.last_popup_rect = None;
  }

  pub fn is_open(&self) -> bool {
    self.open.is_some()
  }

  pub fn popup_rect(&self) -> Option<Rect> {
    self.last_popup_rect
  }

  pub fn choose_item(&self, item_index: usize) -> Option<SelectDropdownChoice> {
    let open = self.open.as_ref()?;
    let item = open.control.items.get(item_index)?;
    match item {
      SelectItem::OptGroupLabel { .. } => None,
      SelectItem::Option {
        disabled, node_id, ..
      } => {
        if *disabled {
          return None;
        }
        Some(SelectDropdownChoice::new(open.select_node_id, *node_id))
      }
    }
  }

  #[cfg(feature = "browser_ui")]
  pub fn ui(&mut self, ctx: &egui::Context) -> Option<SelectDropdownChoice> {
    let Some(open) = self.open.clone() else {
      self.last_popup_rect = None;
      return None;
    };

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.close();
      return None;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)) {
      let choice = selected_choice(open.select_node_id, &open.control);
      self.close();
      return choice;
    }

    let pos = if let Some(anchor) = open.anchor {
      egui::pos2(anchor.x(), anchor.max_y())
    } else {
      ctx
        .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.latest_pos()))
        .unwrap_or_else(|| egui::pos2(0.0, 0.0))
    };

    let id = egui::Id::new(("select_dropdown", open.select_node_id));
    let area = egui::Area::new(id)
      .order(egui::Order::Foreground)
      .fixed_pos(pos);

    let mut choice: Option<SelectDropdownChoice> = None;
    let inner = area.show(ctx, |ui| {
      egui::Frame::popup(ui.style()).show(ui, |ui| {
        if let Some(anchor) = open.anchor {
          let min_width = anchor.width().max(200.0);
          if min_width.is_finite() && min_width > 0.0 {
            ui.set_min_width(min_width);
          }
        }

        egui::ScrollArea::vertical()
          .max_height(240.0)
          .show(ui, |ui| {
            for (idx, item) in open.control.items.iter().enumerate() {
              match item {
                SelectItem::OptGroupLabel { label, disabled } => {
                  ui.add_space(4.0);
                  let text = egui::RichText::new(label).strong();
                  if *disabled {
                    ui.add_enabled(false, egui::Label::new(text));
                  } else {
                    ui.label(text);
                  }
                  ui.add_space(2.0);
                }
                SelectItem::Option {
                  label,
                  value,
                  selected,
                  disabled,
                  in_optgroup,
                  ..
                } => {
                  let base = if label.trim().is_empty() {
                    value
                  } else {
                    label
                  };
                  let text = if *in_optgroup {
                    format!("  {base}")
                  } else {
                    base.to_string()
                  };

                  let response =
                    ui.add_enabled(!*disabled, egui::SelectableLabel::new(*selected, text));
                  if response.clicked() {
                    choice = self.choose_item(idx);
                  }
                }
              }
            }
          });
      });
    });

    if choice.is_some() {
      self.close();
      return choice;
    }

    self.last_popup_rect = Some(Rect::from_xywh(
      inner.response.rect.min.x,
      inner.response.rect.min.y,
      inner.response.rect.width(),
      inner.response.rect.height(),
    ));

    let clicked_outside = ctx.input(|i| {
      i.pointer.any_pressed()
        && i
          .pointer
          .interact_pos()
          .or_else(|| i.pointer.latest_pos())
          .is_some_and(|pos| !inner.response.rect.contains(pos))
    });
    if clicked_outside {
      self.close();
    }

    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  fn sample_control() -> SelectControl {
    SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(vec![
        SelectItem::OptGroupLabel {
          label: "Group".to_string(),
          disabled: false,
        },
        SelectItem::Option {
          node_id: 101,
          label: "One".to_string(),
          value: "1".to_string(),
          selected: false,
          disabled: false,
          in_optgroup: true,
        },
        SelectItem::Option {
          node_id: 102,
          label: "Two".to_string(),
          value: "2".to_string(),
          selected: true,
          disabled: true,
          in_optgroup: true,
        },
      ]),
      selected: vec![2],
    }
  }

  fn nav_control() -> SelectControl {
    SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(vec![
        SelectItem::Option {
          node_id: 1,
          label: "A".to_string(),
          value: "a".to_string(),
          selected: true,
          disabled: false,
          in_optgroup: false,
        },
        SelectItem::OptGroupLabel {
          label: "Group".to_string(),
          disabled: false,
        },
        SelectItem::Option {
          node_id: 2,
          label: "B".to_string(),
          value: "b".to_string(),
          selected: false,
          disabled: true,
          in_optgroup: true,
        },
        SelectItem::Option {
          node_id: 3,
          label: "C".to_string(),
          value: "c".to_string(),
          selected: false,
          disabled: false,
          in_optgroup: true,
        },
      ]),
      selected: vec![0],
    }
  }

  #[test]
  fn selected_choice_returns_none_for_disabled_selected_option() {
    let control = sample_control();
    assert_eq!(selected_choice(10, &control), None);
  }

  #[test]
  fn selected_choice_skips_disabled_selected_options() {
    let mut control = sample_control();
    // Make the disabled option the "active" selected item, but keep a prior enabled selection.
    control.selected = vec![1, 2];
    assert_eq!(
      selected_choice(10, &control),
      Some(SelectDropdownChoice::new(10, 101))
    );
  }

  #[test]
  fn open_close_transitions() {
    let mut dropdown = SelectDropdown::default();
    assert!(!dropdown.is_open());
    dropdown.open(10, sample_control(), None);
    assert!(dropdown.is_open());
    dropdown.close();
    assert!(!dropdown.is_open());
  }

  #[test]
  fn choose_item_returns_choice_for_enabled_options() {
    let mut dropdown = SelectDropdown::default();
    dropdown.open(10, sample_control(), None);

    let choice = dropdown.choose_item(1).expect("expected selectable option");
    assert_eq!(choice, SelectDropdownChoice::new(10, 101));
  }

  #[test]
  fn choose_item_returns_none_for_optgroup_and_disabled_options() {
    let mut dropdown = SelectDropdown::default();
    dropdown.open(10, sample_control(), None);

    assert_eq!(dropdown.choose_item(0), None);
    assert_eq!(dropdown.choose_item(2), None);
  }

  #[test]
  fn next_enabled_option_item_index_moves_across_visible_enabled_options() {
    let control = nav_control();

    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::ArrowDown),
      Some(3),
      "ArrowDown should skip optgroup labels + disabled options"
    );
    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::ArrowUp),
      Some(0),
      "ArrowUp should clamp to the first enabled option"
    );
    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::Home),
      Some(0)
    );
    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::End),
      Some(3)
    );
  }

  #[test]
  fn next_enabled_option_item_index_handles_disabled_selected_anchor() {
    let mut control = nav_control();
    // Make the disabled option the selected anchor (e.g. DOM explicitly selected a disabled option).
    control.selected = vec![2];

    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::ArrowDown),
      Some(3)
    );
    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::ArrowUp),
      Some(0)
    );
  }

  #[test]
  fn next_enabled_option_item_index_returns_none_when_no_enabled_options_exist() {
    let control = SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(vec![SelectItem::Option {
        node_id: 1,
        label: "Only".to_string(),
        value: "only".to_string(),
        selected: true,
        disabled: true,
        in_optgroup: false,
      }]),
      selected: vec![0],
    };

    assert_eq!(
      next_enabled_option_item_index(&control, KeyAction::ArrowDown),
      None
    );
  }
}
