#![cfg(feature = "browser_ui")]

use crate::ui::browser_app::BrowserAppState;
use crate::render_control::StageHeartbeat;
use crate::ui::messages::TabId;

#[derive(Debug, Clone)]
pub enum ChromeAction {
  NewTab,
  CloseTab(TabId),
  ActivateTab(TabId),
  NavigateTo(String),
  Back,
  Forward,
  Reload,
  AddressBarFocusChanged(bool),
}

pub fn chrome_ui(ctx: &egui::Context, app: &mut BrowserAppState) -> Vec<ChromeAction> {
  let mut actions = Vec::new();

  // -----------------------------------------------------------------------------
  // Chrome-level keyboard shortcuts
  // -----------------------------------------------------------------------------
  //
  // These are implemented in the egui frame (rather than as winit-level shortcuts) so we don't
  // need to do platform-specific modifier bookkeeping and can respect egui's "text editing"
  // state. Avoid firing shortcuts while the address bar is focused (or whenever egui is actively
  // consuming keyboard input).
  if !app.chrome.address_bar_has_focus && !ctx.wants_keyboard_input() {
    let (new_tab, close_tab, reload, back, forward, tab_delta) = ctx.input(|i| {
      let cmd_or_ctrl = i.modifiers.command || i.modifiers.ctrl;
      let new_tab = cmd_or_ctrl && i.key_pressed(egui::Key::T);
      let close_tab = cmd_or_ctrl && i.key_pressed(egui::Key::W);
      let reload = cmd_or_ctrl && i.key_pressed(egui::Key::R);
      let back = i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft);
      let forward = i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight);
      let tab_delta = (cmd_or_ctrl && i.key_pressed(egui::Key::Tab)).then(|| {
        if i.modifiers.shift { -1isize } else { 1isize }
      });
      (new_tab, close_tab, reload, back, forward, tab_delta)
    });

    if new_tab {
      actions.push(ChromeAction::NewTab);
    }
    if close_tab {
      if let Some(tab_id) = app.active_tab_id() {
        actions.push(ChromeAction::CloseTab(tab_id));
      }
    }
    if reload {
      actions.push(ChromeAction::Reload);
    }
    if back {
      actions.push(ChromeAction::Back);
    }
    if forward {
      actions.push(ChromeAction::Forward);
    }
    if let Some(delta) = tab_delta {
      let active = app.active_tab_id();
      let len = app.tabs.len();
      if len >= 2 {
        if let Some(active) = active {
          if let Some(idx) = app.tabs.iter().position(|t| t.id == active) {
            let new_idx = (idx as isize + delta).rem_euclid(len as isize) as usize;
            actions.push(ChromeAction::ActivateTab(app.tabs[new_idx].id));
          }
        }
      }
    }
  }

  egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
    // Tabs row.
    ui.horizontal_wrapped(|ui| {
      for tab in &app.tabs {
        let is_active = app.active_tab_id() == Some(tab.id);
        let title = tab.display_title();

        if ui.selectable_label(is_active, title).clicked() {
          actions.push(ChromeAction::ActivateTab(tab.id));
        }

        if ui.button("×").clicked() {
          actions.push(ChromeAction::CloseTab(tab.id));
        }

        ui.separator();
      }

      if ui.button("+").clicked() {
        actions.push(ChromeAction::NewTab);
      }
    });

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let (can_back, can_forward, loading, stage, error) = app
        .active_tab()
        .map(|t| (t.can_go_back, t.can_go_forward, t.loading, t.stage, t.error.clone()))
        .unwrap_or((false, false, false, None, None));

      if ui.add_enabled(can_back, egui::Button::new("←")).clicked() {
        actions.push(ChromeAction::Back);
      }
      if ui
        .add_enabled(can_forward, egui::Button::new("→"))
        .clicked()
      {
        actions.push(ChromeAction::Forward);
      }
      if ui.button("⟳").clicked() {
        actions.push(ChromeAction::Reload);
      }

      let address_bar_id = ui.make_persistent_id("address_bar");
      let response = ui.add(
        egui::TextEdit::singleline(&mut app.chrome.address_bar_text)
          .id(address_bar_id)
          .desired_width(f32::INFINITY)
          .hint_text("Enter URL…"),
      );

      if app.chrome.request_focus_address_bar {
        response.request_focus();
        app.chrome.request_focus_address_bar = false;
      }

      if app.chrome.request_select_all_address_bar {
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, address_bar_id) {
          let end = app.chrome.address_bar_text.chars().count();
          state.set_ccursor_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(end),
          )));
          state.store(ctx, address_bar_id);
        }
        app.chrome.request_select_all_address_bar = false;
      }

      let has_focus = response.has_focus();
      if has_focus != app.chrome.address_bar_has_focus {
        app.chrome.address_bar_has_focus = has_focus;
        app.chrome.address_bar_editing = has_focus;
        actions.push(ChromeAction::AddressBarFocusChanged(has_focus));
      }

      if has_focus && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.chrome.address_bar_editing = false;
        app.sync_address_bar_to_active();
        response.surrender_focus();
      }

      if has_focus && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        app.chrome.address_bar_editing = false;
        actions.push(ChromeAction::NavigateTo(app.chrome.address_bar_text.clone()));
        response.surrender_focus();
      }

      if loading {
        ui.add(egui::Spinner::new());
        let stage = stage.filter(|s| *s != StageHeartbeat::Done);
        match stage {
          Some(stage) => ui.label(egui::RichText::new(format!("Loading… {}", stage.as_str())).small()),
          None => ui.label(egui::RichText::new("Loading…").small()),
        };
      }

      if let Some(err) = error.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.label(
          egui::RichText::new("Error")
            .color(egui::Color32::WHITE)
            .background_color(egui::Color32::from_rgb(160, 0, 0)),
        )
        .on_hover_text(err);
      }
    });
  });

  actions
}
