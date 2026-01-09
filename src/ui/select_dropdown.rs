use crate::geometry::Rect;
use crate::tree::box_tree::{SelectControl, SelectItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdownChoice {
  pub select_node_id: usize,
  pub option_node_id: usize,
}

#[derive(Debug, Clone)]
struct OpenSelectDropdown {
  select_node_id: usize,
  control: SelectControl,
  anchor: Option<Rect>,
}

#[derive(Debug, Default, Clone)]
pub struct SelectDropdown {
  open: Option<OpenSelectDropdown>,
}

impl SelectDropdown {
  pub fn open(&mut self, select_node_id: usize, control: SelectControl, anchor: Option<Rect>) {
    self.open = Some(OpenSelectDropdown {
      select_node_id,
      control,
      anchor,
    });
  }

  pub fn close(&mut self) {
    self.open = None;
  }

  pub fn is_open(&self) -> bool {
    self.open.is_some()
  }

  pub fn choose_item(&self, item_index: usize) -> Option<SelectDropdownChoice> {
    let open = self.open.as_ref()?;
    let item = open.control.items.get(item_index)?;
    match item {
      SelectItem::OptGroupLabel { .. } => None,
      SelectItem::Option {
        disabled,
        node_id,
        ..
      } => {
        if *disabled {
          return None;
        }
        Some(SelectDropdownChoice {
          select_node_id: open.select_node_id,
          option_node_id: *node_id,
        })
      }
    }
  }

  #[cfg(feature = "browser_ui")]
  pub fn ui(&mut self, ctx: &egui::Context) -> Option<SelectDropdownChoice> {
    let Some(open) = self.open.clone() else {
      return None;
    };

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.close();
      return None;
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
          ui.set_min_width(anchor.width());
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
          for (idx, item) in open.control.items.iter().enumerate() {
            match item {
              SelectItem::OptGroupLabel { label, .. } => {
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new(label).strong()));
                ui.add_space(2.0);
              }
              SelectItem::Option {
                label,
                selected,
                disabled,
                in_optgroup,
                ..
              } => {
                let response = if *in_optgroup {
                  ui.add_enabled(
                    !*disabled,
                    egui::SelectableLabel::new(*selected, format!("  {label}")),
                  )
                } else {
                  ui.add_enabled(!*disabled, egui::SelectableLabel::new(*selected, label.as_str()))
                };
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
    assert_eq!(
      choice,
      SelectDropdownChoice {
        select_node_id: 10,
        option_node_id: 101,
      }
    );
  }

  #[test]
  fn choose_item_returns_none_for_optgroup_and_disabled_options() {
    let mut dropdown = SelectDropdown::default();
    dropdown.open(10, sample_control(), None);

    assert_eq!(dropdown.choose_item(0), None);
    assert_eq!(dropdown.choose_item(2), None);
  }
}
