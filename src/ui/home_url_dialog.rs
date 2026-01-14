#![cfg(feature = "browser_ui")]

//! "Set home page" dialog UI for the windowed browser frontend.
//!
//! This dialog is intentionally "pure UI": it renders widgets and returns structured outputs that
//! capture user intent. Side effects (persisting the new home page URL, autosave) are performed by
//! the caller (typically `src/bin/browser.rs`).

use super::{panel_header, BrowserIcon};

#[derive(Debug, Default)]
pub struct HomeUrlDialogOutput {
  /// A validated + normalized URL to persist as the new home page.
  pub save_url: Option<String>,
}

pub fn home_url_dialog_ui(
  ctx: &egui::Context,
  open: &mut bool,
  url_text: &mut String,
  error: &mut Option<String>,
  request_initial_focus: &mut bool,
) -> HomeUrlDialogOutput {
  let mut out = HomeUrlDialogOutput::default();

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
  let mut save_button_id: Option<egui::Id> = None;
  let mut cancel_button_id: Option<egui::Id> = None;

  // Backdrop (modal scrim). Draw behind the window to dim the underlying UI and intercept pointer
  // events so the dialog feels modal.
  let screen_rect = ctx.screen_rect();
  egui::Area::new("home_url_dialog_backdrop")
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

  let input_id = egui::Id::new("home_url_dialog_input");

  egui::Window::new("Set home page")
    .collapsible(false)
    .resizable(false)
    .title_bar(false)
    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
    .open(open)
    .show(ctx, |ui| {
      ui.set_min_width(520.0);

      let header = panel_header(ui, BrowserIcon::Home, "Set home page", || {
        close_dialog = true;
      });
      close_button_id = Some(header.close_response.id);
      #[cfg(test)]
      store_test_id(ctx, "home_url_dialog_close_id", header.close_response.id);
      ui.add_space(10.0);

      ui.label("Enter the URL to open when you click the Home button.");
      ui.label(
        egui::RichText::new("Allowed: about:, http(s)://, file://, or a file path.")
          .small()
          .weak(),
      );
      ui.add_space(10.0);

      // Apply focus/select-all requests before building the `TextEdit` so the first keypress
      // (including Enter) works reliably in the same frame.
      if request_initial_focus {
        ui.memory_mut(|mem| mem.request_focus(input_id));
        let end = url_text.chars().count();
        let mut state = egui::text_edit::TextEditState::load(ctx, input_id).unwrap_or_default();
        state.set_ccursor_range(Some(egui::text::CCursorRange::two(
          egui::text::CCursor::new(0),
          egui::text::CCursor::new(end),
        )));
        state.store(ctx, input_id);
      }

      let resp = ui.add(
        egui::TextEdit::singleline(url_text)
          .id(input_id)
          .desired_width(f32::INFINITY)
          .hint_text("about:newtab")
          .margin(egui::Vec2::new(8.0, 8.0)),
      );
      resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Home page URL"));
      #[cfg(test)]
      store_test_id(ctx, "home_url_dialog_input_id", resp.id);
      if resp.changed() {
        *error = None;
      }

      // Enter while focused on the input acts like Save.
      let mut save_requested = false;
      if resp.has_focus() {
        ui.input_mut(|i| {
          save_requested = i.consume_key(Default::default(), egui::Key::Enter);
        });
      }

      if let Some(err) = error.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(err).color(ui.visuals().error_fg_color));
      }

      ui.add_space(14.0);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let save = ui.button("Save");
        save.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Save home page"));
        save_button_id = Some(save.id);
        #[cfg(test)]
        store_test_rect(ctx, "home_url_dialog_save_button_rect", save.rect);
        #[cfg(test)]
        store_test_id(ctx, "home_url_dialog_save_id", save.id);
        if save.clicked() {
          save_requested = true;
        }

        let cancel = ui.button("Cancel");
        cancel.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Cancel"));
        cancel_button_id = Some(cancel.id);
        #[cfg(test)]
        store_test_id(ctx, "home_url_dialog_cancel_id", cancel.id);
        if cancel.clicked() {
          close_dialog = true;
        }

        if save_requested {
          let trimmed = crate::ui::url::trim_ascii_whitespace(url_text);
          if trimmed.is_empty() {
            out.save_url = Some(crate::ui::about_pages::ABOUT_NEWTAB.to_string());
            close_dialog = true;
          } else {
            match crate::ui::normalize_user_typed_navigation_url(trimmed, None) {
              Ok(normalized) => {
                out.save_url = Some(normalized);
                close_dialog = true;
              }
              Err(err) => {
                *error = Some(err);
              }
            }
          }
        }
      });
    });

  // Apply focus trapping once the dialog controls have been built so we have their IDs.
  if *open {
    let mut focus_order = Vec::new();
    if let Some(id) = close_button_id {
      focus_order.push(id);
    }
    focus_order.push(input_id);
    if let Some(id) = save_button_id {
      focus_order.push(id);
    }
    if let Some(id) = cancel_button_id {
      focus_order.push(id);
    }

    if !focus_order.is_empty() {
      // Prefer to start focus on the URL input.
      let first_id = input_id;
      let last_id = cancel_button_id
        .or_else(|| focus_order.last().copied())
        .unwrap_or(first_id);

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

#[cfg(test)]
fn store_test_rect(ctx: &egui::Context, key: &'static str, rect: egui::Rect) {
  ctx.data_mut(|d| {
    d.insert_temp(egui::Id::new(key), rect);
  });
}

#[cfg(test)]
fn store_test_id(ctx: &egui::Context, key: &'static str, id: egui::Id) {
  ctx.data_mut(|d| {
    d.insert_temp(egui::Id::new(key), id);
  });
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::{home_url_dialog_ui, HomeUrlDialogOutput};
  use crate::ui::a11y_test_util;

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn expect_temp_rect(ctx: &egui::Context, key: &'static str) -> egui::Rect {
    ctx
      .data(|d| d.get_temp::<egui::Rect>(egui::Id::new(key)))
      .unwrap_or_else(|| panic!("expected temp rect {key:?}"))
  }

  fn left_click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    vec![
      egui::Event::PointerMoved(pos),
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
      },
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
      },
    ]
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

  fn outside_ui(ctx: &egui::Context, request_focus: bool) -> egui::Id {
    let mut out: Option<egui::Id> = None;
    egui::CentralPanel::default().show(ctx, |ui| {
      let resp = ui.button("Outside");
      if request_focus {
        resp.request_focus();
      }
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
      expect_temp_id(ctx, "home_url_dialog_close_id"),
      expect_temp_id(ctx, "home_url_dialog_input_id"),
      expect_temp_id(ctx, "home_url_dialog_save_id"),
      expect_temp_id(ctx, "home_url_dialog_cancel_id"),
    ]
  }

  #[test]
  fn save_emits_normalized_url_and_closes_dialog() {
    let ctx = egui::Context::default();
    let mut open = true;
    let mut url_text = "example.com".to_string();
    let mut error: Option<String> = None;
    let mut request_focus = true;

    // Frame 1: render to discover the save button rect.
    begin_frame(&ctx, Vec::new());
    let _ = home_url_dialog_ui(
      &ctx,
      &mut open,
      &mut url_text,
      &mut error,
      &mut request_focus,
    );
    let _ = ctx.end_frame();
    assert!(open, "dialog should still be open after first frame");
    let save_rect = expect_temp_rect(&ctx, "home_url_dialog_save_button_rect");

    // Frame 2: click Save.
    begin_frame(&ctx, left_click_at(save_rect.center()));
    let out: HomeUrlDialogOutput = home_url_dialog_ui(
      &ctx,
      &mut open,
      &mut url_text,
      &mut error,
      &mut request_focus,
    );
    let _ = ctx.end_frame();

    assert_eq!(out.save_url.as_deref(), Some("https://example.com/"));
    assert!(!open, "dialog should close after successful save");
  }

  #[test]
  fn home_url_dialog_traps_tab_focus_inside_modal() {
    let ctx = egui::Context::default();
    let mut open = true;
    let mut url_text = "example.com".to_string();
    let mut error: Option<String> = None;
    // Intentionally *do not* request initial focus to ensure the dialog traps focus even if it was
    // opened while another widget had it.
    let mut request_initial_focus = false;

    // Frame 0: focus the outside control and render the dialog once.
    begin_frame(&ctx, Vec::new());
    let outside_id = outside_ui(&ctx, true);
    let _ = home_url_dialog_ui(
      &ctx,
      &mut open,
      &mut url_text,
      &mut error,
      &mut request_initial_focus,
    );
    let _ = ctx.end_frame();

    assert!(open, "dialog should remain open");
    assert!(
      !ctx.memory(|mem| mem.has_focus(outside_id)),
      "expected focus to move off the outside widget when the dialog opens"
    );

    // Subsequent frames: press Tab repeatedly. Focus must never escape to the outside control.
    for step in 0..12 {
      begin_frame(&ctx, vec![key_press(egui::Key::Tab)]);
      let outside_id_now = outside_ui(&ctx, false);
      let _ = home_url_dialog_ui(
        &ctx,
        &mut open,
        &mut url_text,
        &mut error,
        &mut request_initial_focus,
      );
      let _ = ctx.end_frame();

      assert!(open, "dialog should remain open during Tab trapping test");

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
      let outside_id_now = outside_ui(&ctx, false);
      let _ = home_url_dialog_ui(
        &ctx,
        &mut open,
        &mut url_text,
        &mut error,
        &mut request_initial_focus,
      );
      let _ = ctx.end_frame();

      assert!(open, "dialog should remain open during Shift+Tab trapping test");

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
  fn home_url_dialog_emits_accesskit_names_for_primary_controls() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut open = true;
    let mut url_text = "example.com".to_string();
    let mut error: Option<String> = None;
    let mut request_initial_focus = true;

    begin_frame(&ctx, Vec::new());
    let _outside = outside_ui(&ctx, false);
    let _ = home_url_dialog_ui(
      &ctx,
      &mut open,
      &mut url_text,
      &mut error,
      &mut request_initial_focus,
    );
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);

    for expected in ["Home page URL", "Save home page", "Cancel"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in home URL dialog output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }
}
