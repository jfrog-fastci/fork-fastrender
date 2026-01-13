#![cfg(feature = "browser_ui")]

//! "Clear browsing data" dialog UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (actually clearing history, persistence) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{icon_tinted, BrowserIcon, ClearBrowsingDataRange};

#[derive(Debug, Default)]
pub struct ClearBrowsingDataDialogOutput {
  pub clear_now: bool,
}

pub fn clear_browsing_data_dialog_ui(
  ctx: &egui::Context,
  open: &mut bool,
  range: &mut ClearBrowsingDataRange,
) -> ClearBrowsingDataDialogOutput {
  let mut out = ClearBrowsingDataDialogOutput::default();

  if !*open {
    return out;
  }

  // Esc closes the dialog (Chrome-like).
  if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
    *open = false;
    return out;
  }

  let mut close_dialog = false;

  egui::Window::new("Clear browsing data")
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
    .open(open)
    .show(ctx, |ui| {
      fn with_alpha(color: egui::Color32, alpha: u8) -> egui::Color32 {
        let [r, g, b, _] = color.to_array();
        egui::Color32::from_rgba_unmultiplied(r, g, b, alpha)
      }

      // Header
      ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        icon_tinted(ui, BrowserIcon::History, 20.0, ui.visuals().warn_fg_color);
        ui.heading("Clear browsing data");
      });
      ui.add_space(6.0);
      ui.label(
        egui::RichText::new("Clear browsing data for this profile.")
          .color(ui.visuals().weak_text_color()),
      );

      ui.add_space(14.0);

      // Time range selection.
      ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Time range").strong());
        ui.add_space(8.0);
        egui::ComboBox::from_id_source("clear_browsing_data_range_combo")
          .selected_text(range.label())
          .width(ui.available_width().min(220.0))
          .show_ui(ui, |ui| {
            ui.selectable_value(
              range,
              ClearBrowsingDataRange::LastHour,
              ClearBrowsingDataRange::LastHour.label(),
            );
            ui.selectable_value(
              range,
              ClearBrowsingDataRange::Last24Hours,
              ClearBrowsingDataRange::Last24Hours.label(),
            );
            ui.selectable_value(
              range,
              ClearBrowsingDataRange::Last7Days,
              ClearBrowsingDataRange::Last7Days.label(),
            );
            ui.selectable_value(
              range,
              ClearBrowsingDataRange::AllTime,
              ClearBrowsingDataRange::AllTime.label(),
            );
          });
      });

      ui.add_space(12.0);
      ui.group(|ui| {
        ui.label(egui::RichText::new("This will remove:").strong());
        ui.add_space(4.0);
        ui.label(egui::RichText::new("• History panel entries").small());
        ui.label(egui::RichText::new("• Recently visited suggestions").small());
      });

      ui.add_space(14.0);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let danger = ui.visuals().error_fg_color;
        let clear_button = egui::Button::new(egui::RichText::new("Clear").strong().color(danger))
          .fill(with_alpha(danger, 24))
          .stroke(egui::Stroke::new(ui.visuals().widgets.inactive.bg_stroke.width, danger));

        if ui.add(clear_button).clicked() {
          out.clear_now = true;
          close_dialog = true;
        }

        if ui.button("Cancel").clicked() {
          close_dialog = true;
        }
      });
    });

  if close_dialog {
    *open = false;
  }

  out
}
