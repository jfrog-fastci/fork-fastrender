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

#[derive(Default, Debug, Clone, Copy)]
struct ChromeShortcuts {
  new_tab: bool,
  close_tab: bool,
  reload: bool,
  back: bool,
  forward: bool,
  next_tab: bool,
  prev_tab: bool,
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
      let stage = active.and_then(|t| t.stage);

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
        let stage = stage.filter(|s| *s != crate::render_control::StageHeartbeat::Done);
        match stage {
          Some(stage) => ui.label(format!("Loading… {}", stage.as_str())),
          None => ui.label("Loading…"),
        };
      }
    });

    if let Some(active) = app.active_tab() {
      if let Some(err) = active.error.as_ref().filter(|s| !s.trim().is_empty()) {
        ui.separator();
        ui.colored_label(egui::Color32::LIGHT_RED, err);
      }
    }
  });

  let shortcuts = if app.chrome.address_bar_has_focus || ctx.wants_keyboard_input() {
    ChromeShortcuts::default()
  } else {
    ctx.input(|i| ChromeShortcuts {
      new_tab: i.modifiers.command && i.key_pressed(egui::Key::T),
      close_tab: i.modifiers.command && i.key_pressed(egui::Key::W),
      reload: i.modifiers.command && i.key_pressed(egui::Key::R),
      back: i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft),
      forward: i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight),
      next_tab: i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Tab),
      prev_tab: i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::Tab),
    })
  };

  if shortcuts.new_tab {
    actions.push(ChromeAction::NewTab);
  }
  if shortcuts.close_tab {
    if let Some(tab_id) = app.active_tab_id() {
      actions.push(ChromeAction::CloseTab(tab_id));
    }
  }
  if shortcuts.reload {
    actions.push(ChromeAction::Reload);
  }
  if shortcuts.back {
    actions.push(ChromeAction::Back);
  }
  if shortcuts.forward {
    actions.push(ChromeAction::Forward);
  }

  if shortcuts.next_tab || shortcuts.prev_tab {
    let Some(active_id) = app.active_tab_id() else {
      return actions;
    };
    let Some(active_idx) = app.tabs.iter().position(|t| t.id == active_id) else {
      return actions;
    };
    if app.tabs.is_empty() {
      return actions;
    }
    let len = app.tabs.len();
    let next_idx = if shortcuts.prev_tab {
      (active_idx + len - 1) % len
    } else {
      (active_idx + 1) % len
    };
    let target = app.tabs[next_idx].id;
    actions.push(ChromeAction::ActivateTab(target));
  }

  actions
}
