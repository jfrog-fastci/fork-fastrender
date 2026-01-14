use crate::geometry::{Point, Rect};
use crate::interaction::KeyAction;
use crate::tree::box_tree::{SelectControl, SelectItem};

#[path = "select_dropdown/choice.rs"]
mod choice;

pub use choice::SelectDropdownChoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectDropdownPopupDirection {
  /// Dropdown opens below the `<select>` control (preferred when there is space).
  Down,
  /// Dropdown opens above the `<select>` control (used when there is more space above).
  Up,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectDropdownPopupPlacement {
  /// Popup rectangle in the same coordinate space as `screen_rect` and `anchor_rect`.
  ///
  /// Note: this represents the *maximum* rectangle we intend to occupy (e.g. when using a scroll
  /// area). The actual UI may end up shorter if there are few options.
  pub rect: Rect,
  pub direction: SelectDropdownPopupDirection,
}

/// Compute a clamped `<select>` dropdown popup rectangle for a given screen/anchor configuration.
///
/// This is intended for windowed UIs: given a *screen* rectangle and an optional *anchor* rectangle
/// (the `<select>` control's bounds), compute a popup rect that:
/// - aligns to the anchor's left edge when possible,
/// - prefers opening below the anchor,
/// - flips above when there is more space above,
/// - clamps width/height and keeps the popup inside the screen (with padding).
///
/// The returned rectangle uses the same coordinate space as the input (e.g. egui points).
pub fn select_dropdown_popup_placement(
  screen_rect: Rect,
  anchor_rect: Option<Rect>,
  fallback_anchor: Point,
  preferred_width: f32,
  min_width: f32,
  max_width: f32,
  max_height: f32,
  edge_padding: f32,
) -> SelectDropdownPopupPlacement {
  let edge_padding = if edge_padding.is_finite() {
    edge_padding.max(0.0)
  } else {
    0.0
  };

  let screen_left = screen_rect.min_x() + edge_padding;
  let screen_top = screen_rect.min_y() + edge_padding;
  let screen_right = screen_rect.max_x() - edge_padding;
  let screen_bottom = screen_rect.max_y() - edge_padding;

  let available_screen_width = (screen_right - screen_left).max(0.0);
  let available_screen_height = (screen_bottom - screen_top).max(0.0);

  let min_width = if min_width.is_finite() {
    min_width.max(0.0)
  } else {
    0.0
  };
  let max_width = if max_width.is_finite() {
    max_width.max(min_width)
  } else {
    min_width
  };
  let preferred_width = if preferred_width.is_finite() {
    preferred_width.max(0.0)
  } else {
    0.0
  };

  let width = preferred_width
    .clamp(min_width, max_width)
    .min(available_screen_width);

  let max_height = if max_height.is_finite() {
    max_height.max(0.0)
  } else {
    0.0
  };

  let (anchor_left, anchor_top, anchor_bottom) = if let Some(anchor) = anchor_rect {
    (anchor.min_x(), anchor.min_y(), anchor.max_y())
  } else {
    (fallback_anchor.x, fallback_anchor.y, fallback_anchor.y)
  };

  let avail_below = (screen_bottom - anchor_bottom).max(0.0);
  let avail_above = (anchor_top - screen_top).max(0.0);

  let direction = if avail_below >= avail_above {
    SelectDropdownPopupDirection::Down
  } else {
    SelectDropdownPopupDirection::Up
  };

  let available_height = match direction {
    SelectDropdownPopupDirection::Down => avail_below,
    SelectDropdownPopupDirection::Up => avail_above,
  };
  let height = max_height
    .min(available_height)
    .min(available_screen_height);

  let min_x = screen_left;
  let max_x = (screen_right - width).max(min_x);
  let x = if anchor_left.is_finite() {
    anchor_left.clamp(min_x, max_x)
  } else {
    min_x
  };

  let min_y = screen_top;
  let max_y = (screen_bottom - height).max(min_y);
  let y_unclamped = match direction {
    SelectDropdownPopupDirection::Down => anchor_bottom,
    SelectDropdownPopupDirection::Up => anchor_top - height,
  };
  let y = if y_unclamped.is_finite() {
    y_unclamped.clamp(min_y, max_y)
  } else {
    min_y
  };

  SelectDropdownPopupPlacement {
    rect: Rect::from_xywh(x, y, width, height),
    direction,
  }
}

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

  let (first_enabled, last_enabled) = first_last_enabled_option_item_index(control)?;

  // Anchor to the currently-selected option item index (even if disabled), falling back to the
  // first enabled option. We intentionally ignore optgroup labels (they are not selectable).
  let selected_option_idx = control
    .selected
    .last()
    .copied()
    .filter(|idx| control.items.get(*idx).is_some_and(|item| matches!(item, SelectItem::Option { .. })));
  let anchor = selected_option_idx.unwrap_or(first_enabled);

  let next = match key {
    Home => first_enabled,
    End => last_enabled,
    ArrowDown => next_enabled_option_item_index_after(control, anchor).unwrap_or(last_enabled),
    ArrowUp => prev_enabled_option_item_index_before(control, anchor).unwrap_or(first_enabled),
    _ => return None,
  };

  Some(next)
}

/// Move the currently-selected `<option>` by a delta measured in **enabled options**.
///
/// For example:
/// - `delta = 1` moves to the next enabled option (ArrowDown semantics).
/// - `delta = -1` moves to the previous enabled option (ArrowUp semantics).
/// - Larger deltas are useful for PageUp/PageDown-style navigation.
///
/// Returns the **item index** into [`SelectControl::items`] for the option that should become
/// selected, or `None` when there are no enabled options.
pub fn offset_enabled_option_item_index(control: &SelectControl, delta: isize) -> Option<usize> {
  if delta == 0 {
    // Treat "no movement" as a request for the current selection, falling back to the first enabled
    // option.
    return control
      .selected
      .last()
      .copied()
      .or_else(|| next_enabled_option_item_index(control, KeyAction::Home));
  }

  let (first_enabled, last_enabled) = first_last_enabled_option_item_index(control)?;

  let selected_option_idx = control
    .selected
    .last()
    .copied()
    .filter(|idx| control.items.get(*idx).is_some_and(|item| matches!(item, SelectItem::Option { .. })));
  let mut current = selected_option_idx.unwrap_or(first_enabled);

  let step_count = delta.unsigned_abs();
  if delta > 0 {
    for _ in 0..step_count {
      match next_enabled_option_item_index_after(control, current) {
        Some(next) => current = next,
        None => {
          current = last_enabled;
          break;
        }
      }
    }
  } else {
    for _ in 0..step_count {
      match prev_enabled_option_item_index_before(control, current) {
        Some(prev) => current = prev,
        None => {
          current = first_enabled;
          break;
        }
      }
    }
  }

  Some(current)
}

fn first_last_enabled_option_item_index(control: &SelectControl) -> Option<(usize, usize)> {
  let mut first_enabled: Option<usize> = None;
  let mut last_enabled: Option<usize> = None;
  for (idx, item) in control.items.iter().enumerate() {
    let SelectItem::Option { disabled, .. } = item else {
      continue;
    };
    if *disabled {
      continue;
    }
    if first_enabled.is_none() {
      first_enabled = Some(idx);
    }
    last_enabled = Some(idx);
  }

  let first_enabled = first_enabled?;
  let last_enabled = last_enabled.unwrap_or(first_enabled);
  Some((first_enabled, last_enabled))
}

fn next_enabled_option_item_index_after(control: &SelectControl, anchor: usize) -> Option<usize> {
  if anchor >= control.items.len() {
    return None;
  }
  for (idx, item) in control.items.iter().enumerate().skip(anchor + 1) {
    let SelectItem::Option { disabled, .. } = item else {
      continue;
    };
    if !*disabled {
      return Some(idx);
    }
  }
  None
}

fn prev_enabled_option_item_index_before(control: &SelectControl, anchor: usize) -> Option<usize> {
  let end = anchor.min(control.items.len());
  for idx in (0..end).rev() {
    let Some(SelectItem::Option { disabled, .. }) = control.items.get(idx) else {
      continue;
    };
    if !*disabled {
      return Some(idx);
    }
  }
  None
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
  fn popup_placement_prefers_below_when_space_allows() {
    let screen = Rect::from_xywh(0.0, 0.0, 800.0, 600.0);
    let anchor = Rect::from_xywh(100.0, 100.0, 200.0, 20.0);

    let placement = select_dropdown_popup_placement(
      screen,
      Some(anchor),
      Point::new(0.0, 0.0),
      240.0,
      160.0,
      600.0,
      300.0,
      8.0,
    );

    assert_eq!(placement.direction, SelectDropdownPopupDirection::Down);
    assert!(placement.rect.min_x() >= 8.0);
    assert!(placement.rect.min_y() >= anchor.max_y());
    assert!(placement.rect.max_x() <= 800.0 - 8.0 + f32::EPSILON);
    assert!(placement.rect.max_y() <= 600.0 - 8.0 + f32::EPSILON);
    assert_eq!(placement.rect.width(), 240.0);
    assert_eq!(placement.rect.height(), 300.0);
  }

  #[test]
  fn popup_placement_flips_above_when_near_bottom() {
    let screen = Rect::from_xywh(0.0, 0.0, 800.0, 600.0);
    let anchor = Rect::from_xywh(100.0, 580.0, 200.0, 20.0);

    let placement = select_dropdown_popup_placement(
      screen,
      Some(anchor),
      Point::new(0.0, 0.0),
      240.0,
      160.0,
      600.0,
      300.0,
      8.0,
    );

    assert_eq!(placement.direction, SelectDropdownPopupDirection::Up);
    assert!((placement.rect.max_y() - anchor.min_y()).abs() < 1e-3);
    assert!(placement.rect.min_y() >= 8.0 - 1e-3);
  }

  #[test]
  fn popup_placement_clamps_x_and_width_to_screen() {
    let screen = Rect::from_xywh(0.0, 0.0, 300.0, 200.0);
    let anchor = Rect::from_xywh(280.0, 50.0, 30.0, 20.0);

    let placement = select_dropdown_popup_placement(
      screen,
      Some(anchor),
      Point::new(0.0, 0.0),
      500.0,
      160.0,
      600.0,
      300.0,
      8.0,
    );

    // Screen width minus padding = 284, so the popup must clamp.
    assert!(placement.rect.width() <= 284.0 + 1e-3);
    assert!(placement.rect.max_x() <= 300.0 - 8.0 + 1e-3);
    assert!(placement.rect.min_x() >= 8.0 - 1e-3);
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

  #[test]
  fn offset_enabled_option_item_index_moves_by_multiple_enabled_options() {
    let control = nav_control();
    assert_eq!(
      offset_enabled_option_item_index(&control, 2),
      Some(3),
      "delta should skip disabled options and clamp if needed"
    );
    assert_eq!(
      offset_enabled_option_item_index(&control, -1),
      Some(0),
      "delta up should clamp to first enabled option"
    );
  }

  #[test]
  fn offset_enabled_option_item_index_handles_disabled_anchor() {
    let mut control = nav_control();
    control.selected = vec![2];

    assert_eq!(offset_enabled_option_item_index(&control, 1), Some(3));
    assert_eq!(offset_enabled_option_item_index(&control, -1), Some(0));
  }
}
