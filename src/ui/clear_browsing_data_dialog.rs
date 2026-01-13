#![cfg(feature = "browser_ui")]

//! "Clear browsing data" dialog UI for the windowed browser frontend.
//!
//! This module is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (actually clearing history, persistence) are performed by the
//! caller (typically `src/bin/browser.rs`).

use super::{danger_button, panel_header, BrowserIcon, ClearBrowsingDataRange};

#[derive(Debug, Default)]
pub struct ClearBrowsingDataDialogOutput {
  pub clear_now: bool,
}

#[cfg(test)]
fn store_test_id(ctx: &egui::Context, key: &'static str, id: egui::Id) {
  ctx.data_mut(|d| {
    d.insert_temp(egui::Id::new(key), id);
  });
}

pub fn clear_browsing_data_dialog_ui(
  ctx: &egui::Context,
  open: &mut bool,
  range: &mut ClearBrowsingDataRange,
  request_initial_focus: &mut bool,
) -> ClearBrowsingDataDialogOutput {
  let mut out = ClearBrowsingDataDialogOutput::default();

  if !*open {
    *request_initial_focus = false;
    return out;
  }

  let request_initial_focus = std::mem::take(request_initial_focus);

  // Esc closes the dialog (Chrome-like) and should not leak to other overlays.
  let escape_pressed = ctx.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape));
  if escape_pressed {
    *open = false;
    return out;
  }

  // Focus trap: keep Tab/Shift+Tab traversal inside the dialog controls.
  //
  // Egui's default focus traversal walks every focusable widget in the frame. When this dialog is
  // open, we want it to behave like a modal: focus must not escape to the underlying chrome.
  let tab_pressed = ctx.input_mut(|i| i.consume_key(egui::Modifiers::default(), egui::Key::Tab));
  let shift_tab_pressed = ctx.input_mut(|i| {
    i.consume_key(
      egui::Modifiers {
        shift: true,
        ..Default::default()
      },
      egui::Key::Tab,
    )
  });

  let mut close_dialog = false;
  let mut close_button_id: Option<egui::Id> = None;
  let mut range_combo_id: Option<egui::Id> = None;
  let mut clear_button_id: Option<egui::Id> = None;
  let mut cancel_button_id: Option<egui::Id> = None;

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
      ui.painter().rect_filled(
        rect,
        egui::Rounding::none(),
        egui::Color32::from_black_alpha(alpha),
      );
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
      let header = panel_header(ui, BrowserIcon::History, "Clear browsing data", || {
        close_dialog = true;
      });
      close_button_id = Some(header.close_response.id);
      #[cfg(test)]
      store_test_id(ctx, "clear_browsing_data_dialog_close_id", header.close_response.id);
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
        combo
          .response
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Time range"));
        range_combo_id = Some(combo.response.id);
        #[cfg(test)]
        store_test_id(ctx, "clear_browsing_data_dialog_time_range_id", combo.response.id);
        if request_initial_focus {
          combo.response.request_focus();
        }
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
        clear_button_id = Some(clear_resp.id);
        #[cfg(test)]
        store_test_id(ctx, "clear_browsing_data_dialog_clear_id", clear_resp.id);
        if clear_resp.clicked() {
          out.clear_now = true;
          close_dialog = true;
        }

        let cancel_resp = ui.button("Cancel");
        cancel_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Cancel"));
        cancel_button_id = Some(cancel_resp.id);
        #[cfg(test)]
        store_test_id(ctx, "clear_browsing_data_dialog_cancel_id", cancel_resp.id);
        if cancel_resp.clicked() {
          close_dialog = true;
        }
      });
    });

  // Apply focus trapping once the dialog controls have been built so we have their IDs.
  if *open {
    let mut focus_order = Vec::new();
    if let Some(id) = close_button_id {
      focus_order.push(id);
    }
    if let Some(id) = range_combo_id {
      focus_order.push(id);
    }
    if let Some(id) = clear_button_id {
      focus_order.push(id);
    }
    if let Some(id) = cancel_button_id {
      focus_order.push(id);
    }

    if !focus_order.is_empty() {
      // Prefer to start focus on the time range control (matches initial-focus behavior).
      let first_id = range_combo_id.unwrap_or(focus_order[0]);
      let last_id = cancel_button_id.or_else(|| focus_order.last().copied()).unwrap_or(first_id);

      let focused = ctx.memory(|mem| mem.focus());
      let focused_idx = focused.and_then(|id| focus_order.iter().position(|f| *f == id));

      let mut request_focus: Option<egui::Id> = None;
      if tab_pressed || shift_tab_pressed {
        request_focus = Some(match (focused_idx, shift_tab_pressed) {
          (Some(idx), false) => focus_order[(idx + 1) % focus_order.len()],
          (Some(idx), true) => {
            let prev = if idx == 0 {
              focus_order.len() - 1
            } else {
              idx - 1
            };
            focus_order[prev]
          }
          (None, false) => first_id,
          (None, true) => last_id,
        });
      } else if !request_initial_focus {
        // If focus somehow escaped (e.g. dialog opened without requesting initial focus), pull it
        // back into the modal.
        if focused_idx.is_none() {
          request_focus = Some(first_id);
        }
      }

      if let Some(id) = request_focus {
        ctx.memory_mut(|mem| mem.request_focus(id));
      }
    }
  }

  if close_dialog {
    *open = false;
  }

  out
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::clear_browsing_data_dialog_ui;
  use crate::ui::a11y_test_util;
  use crate::ui::ClearBrowsingDataRange;

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn key_press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
      key,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers::default(),
    }
  }

  fn shift_tab_press() -> egui::Event {
    egui::Event::Key {
      key: egui::Key::Tab,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }
  }

  fn outside_ui(ctx: &egui::Context) -> egui::Id {
    let mut out: Option<egui::Id> = None;
    egui::CentralPanel::default().show(ctx, |ui| {
      let resp = ui.button("Outside");
      out = Some(resp.id);
    });
    out.expect("outside button must be created")
  }

  fn expect_temp_id(ctx: &egui::Context, key: &'static str) -> egui::Id {
    ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new(key)))
      .unwrap_or_else(|| panic!("expected temp id {key:?}"))
  }

  fn dialog_ids(ctx: &egui::Context) -> Vec<egui::Id> {
    vec![
      expect_temp_id(ctx, "clear_browsing_data_dialog_close_id"),
      expect_temp_id(ctx, "clear_browsing_data_dialog_time_range_id"),
      expect_temp_id(ctx, "clear_browsing_data_dialog_clear_id"),
      expect_temp_id(ctx, "clear_browsing_data_dialog_cancel_id"),
    ]
  }

  #[test]
  fn clear_browsing_data_dialog_traps_tab_focus_inside_modal() {
    let ctx = egui::Context::default();
    let mut open = true;
    let mut range = ClearBrowsingDataRange::default();
    let mut request_initial_focus = true;

    // Frame 0: render the UI once to capture widget ids and apply initial focus.
    begin_frame(&ctx, Vec::new());
    let outside_id = outside_ui(&ctx);
    let _ = clear_browsing_data_dialog_ui(&ctx, &mut open, &mut range, &mut request_initial_focus);
    let _ = ctx.end_frame();

    assert!(open, "dialog should remain open");
    let ids = dialog_ids(&ctx);
    let time_range_id = ids[1];
    assert!(
      ctx.memory(|mem| mem.has_focus(time_range_id)),
      "expected initial focus on time range combo"
    );

    // Subsequent frames: press Tab repeatedly. Focus must never escape to the outside control.
    for step in 0..12 {
      begin_frame(&ctx, vec![key_press(egui::Key::Tab)]);
      let outside_id_now = outside_ui(&ctx);
      let _ = clear_browsing_data_dialog_ui(&ctx, &mut open, &mut range, &mut request_initial_focus);
      let _ = ctx.end_frame();

      let ids = dialog_ids(&ctx);
      let focused = ids
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));

      assert!(
        focused.is_some(),
        "focus escaped the dialog after Tab step {step}; focus ids: {ids:?}, outside id: {outside_id:?}"
      );
      assert!(
        !ctx.memory(|mem| mem.has_focus(outside_id_now)),
        "focus escaped to outside control after Tab step {step}"
      );
    }

    // Also verify Shift+Tab stays inside.
    for step in 0..12 {
      begin_frame(&ctx, vec![shift_tab_press()]);
      let outside_id_now = outside_ui(&ctx);
      let _ = clear_browsing_data_dialog_ui(&ctx, &mut open, &mut range, &mut request_initial_focus);
      let _ = ctx.end_frame();

      let ids = dialog_ids(&ctx);
      let focused = ids
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));

      assert!(
        focused.is_some(),
        "focus escaped the dialog after Shift+Tab step {step}; focus ids: {ids:?}, outside id: {outside_id:?}"
      );
      assert!(
        !ctx.memory(|mem| mem.has_focus(outside_id_now)),
        "focus escaped to outside control after Shift+Tab step {step}"
      );
    }
  }

  #[test]
  fn clear_browsing_data_dialog_emits_accesskit_names_for_primary_controls() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut open = true;
    let mut range = ClearBrowsingDataRange::default();
    let mut request_initial_focus = true;

    begin_frame(&ctx, Vec::new());
    let _ = outside_ui(&ctx);
    let _ = clear_browsing_data_dialog_ui(&ctx, &mut open, &mut range, &mut request_initial_focus);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);

    for expected in ["Time range", "Clear browsing data", "Cancel"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in clear browsing data dialog output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }
}
