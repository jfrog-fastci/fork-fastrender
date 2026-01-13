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

  let mut close_dialog = false;

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

      panel_header(ui, BrowserIcon::Home, "Set home page", || {
        close_dialog = true;
      });
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
        #[cfg(test)]
        store_test_rect(ctx, "home_url_dialog_save_button_rect", save.rect);
        if save.clicked() {
          save_requested = true;
        }

        let cancel = ui.button("Cancel");
        cancel.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Cancel"));
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

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::{home_url_dialog_ui, HomeUrlDialogOutput};

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
}
