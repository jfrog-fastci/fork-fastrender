#![cfg(feature = "browser_ui")]

//! "Clear browsing data" dialog UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (actually clearing history, persistence) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{danger_button, icon_button, icon_tinted, BrowserIcon, ClearBrowsingDataRange};

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

  // Backdrop (modal scrim). Draw behind the window to dim the underlying UI and intercept pointer
  // events so the dialog feels modal.
  let screen_rect = ctx.screen_rect();
  egui::Area::new("clear_browsing_data_backdrop")
    .order(egui::Order::Middle)
    .fixed_pos(screen_rect.min)
    .show(ctx, |ui| {
      ui.set_min_size(screen_rect.size());
      let (rect, _resp) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());
      let alpha = if ui.visuals().dark_mode { 140 } else { 96 };
      ui
        .painter()
        .rect_filled(rect, egui::Rounding::none(), egui::Color32::from_black_alpha(alpha));
    });

  egui::Window::new("Clear browsing data")
    .collapsible(false)
    .resizable(false)
    .title_bar(false)
    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
    .open(open)
    .show(ctx, |ui| {
      ui.set_min_width(420.0);

      // Header (custom title bar)
      ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        icon_tinted(ui, BrowserIcon::History, 20.0, ui.visuals().warn_fg_color);
        ui.heading("Clear browsing data");

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          let close_resp = icon_button(ui, BrowserIcon::Close, "Close (Esc)", true);
          if close_resp.clicked() {
            close_dialog = true;
          }
        });
      });
      ui.add_space(8.0);
      ui.label(
        egui::RichText::new("Clear browsing data for this profile.")
          .color(ui.visuals().weak_text_color()),
      );

      ui.add_space(14.0);

      // Time range selection.
      ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Time range").strong());
        ui.add_space(8.0);
        let combo = egui::ComboBox::from_id_source("clear_browsing_data_range_combo")
          .selected_text(range.label())
          .width(ui.available_width().min(220.0))
          .show_ui(ui, |ui| {
            let last_hour = ui.selectable_value(
              range,
              ClearBrowsingDataRange::LastHour,
              ClearBrowsingDataRange::LastHour.label(),
            );
            last_hour.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range: Last hour")
            });
            let last_24 = ui.selectable_value(
              range,
              ClearBrowsingDataRange::Last24Hours,
              ClearBrowsingDataRange::Last24Hours.label(),
            );
            last_24.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range: Last 24 hours")
            });
            let last_7 = ui.selectable_value(
              range,
              ClearBrowsingDataRange::Last7Days,
              ClearBrowsingDataRange::Last7Days.label(),
            );
            last_7.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range: Last 7 days")
            });
            let all_time = ui.selectable_value(
              range,
              ClearBrowsingDataRange::AllTime,
              ClearBrowsingDataRange::AllTime.label(),
            );
            all_time.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range: All time")
            });
          });
        combo.response.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range")
        });
      });

      ui.add_space(12.0);
      ui.group(|ui| {
        ui.label(egui::RichText::new("This will remove:").strong());
        ui.add_space(4.0);
        ui.label(egui::RichText::new("• History panel entries").small());
        ui.label(egui::RichText::new("• Recently visited suggestions").small());
      });
      ui.add_space(6.0);
      ui.label(
        egui::RichText::new("This action cannot be undone.")
          .small()
          .color(ui.visuals().weak_text_color()),
      );

      ui.add_space(14.0);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let clear_resp = danger_button(ui, "Clear");
        clear_resp.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear browsing data")
        });
        if clear_resp.clicked() {
          out.clear_now = true;
          close_dialog = true;
        }

        let cancel_resp = ui.button("Cancel");
        cancel_resp
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Cancel"));
        if cancel_resp.clicked() {
          close_dialog = true;
        }
      });
    });

  if close_dialog {
    *open = false;
  }

  out
}
