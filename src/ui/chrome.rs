#![cfg(feature = "browser_ui")]

use crate::ui::browser_app::BrowserAppState;
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
      let active = app.active_tab();
      let (can_back, can_forward, loading) = active
        .map(|t| (t.can_go_back, t.can_go_forward, t.loading))
        .unwrap_or((false, false, false));

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

      let response = ui.add(
        egui::TextEdit::singleline(&mut app.chrome.address_bar_text)
          .desired_width(f32::INFINITY)
          .hint_text("Enter URL…"),
      );

      let has_focus = response.has_focus();
      if has_focus != app.chrome.address_bar_has_focus {
        app.chrome.address_bar_has_focus = has_focus;
        actions.push(ChromeAction::AddressBarFocusChanged(has_focus));
      }

      if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        actions.push(ChromeAction::NavigateTo(app.chrome.address_bar_text.clone()));
      }

      if loading {
        ui.label("Loading…");
      }
    });

    if let Some(active) = app.active_tab() {
      if let Some(err) = active.error.as_ref().filter(|s| !s.trim().is_empty()) {
        ui.separator();
        ui.colored_label(egui::Color32::LIGHT_RED, err);
      }
    }
  });

  actions
}
