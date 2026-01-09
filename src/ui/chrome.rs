#![cfg(feature = "browser_ui")]

use crate::ui::browser_app::BrowserAppState;
use crate::render_control::StageHeartbeat;
use crate::ui::messages::TabId;

#[derive(Debug, Clone)]
pub enum ChromeAction {
  /// Focus the address bar and select all contents.
  ///
  /// This is emitted by chrome-level keyboard shortcuts (e.g. Ctrl/Cmd+L).
  ///
  /// Front-ends are expected to translate this into UI state changes (e.g. setting
  /// `ChromeState::request_focus_address_bar` / `request_select_all_address_bar`).
  FocusAddressBar,
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
  // state.
  //
  // Most "browser chrome" shortcuts should still work while the user is editing the address bar,
  // matching typical browser behaviour. Some shortcuts (e.g. Alt+Left/Right) are suppressed while
  // editing to avoid interfering with word-wise cursor movement on some platforms.
  let (focus_address_bar, new_tab, close_tab, reload, tab_delta) = ctx.input(|i| {
    // Use the key event's modifier snapshot rather than `i.modifiers`: the winit integration feeds
    // modifiers via events, and using the event snapshot keeps this robust in unit tests as well.
    let mut focus_address_bar = false;
    let mut new_tab = false;
    let mut close_tab = false;
    let mut reload = false;
    let mut tab_delta: Option<isize> = None;

    for event in &i.events {
      let egui::Event::Key {
        key,
        pressed: true,
        repeat: false,
        modifiers,
      } = event
      else {
        continue;
      };

      // Guard against AltGr (often encoded as Ctrl+Alt).
      let cmd_or_ctrl = (modifiers.command || modifiers.ctrl) && !modifiers.alt;

      match key {
        egui::Key::L if cmd_or_ctrl => focus_address_bar = true,
        egui::Key::T if cmd_or_ctrl => new_tab = true,
        egui::Key::W if cmd_or_ctrl => close_tab = true,
        egui::Key::R if cmd_or_ctrl => reload = true,
        egui::Key::Tab if cmd_or_ctrl => {
          tab_delta = Some(if modifiers.shift { -1isize } else { 1isize })
        }
        // F5 is a common reload shortcut. Ignore modified F5 (Ctrl/Cmd+F5 / Alt+F5).
        egui::Key::F5 if !(modifiers.command || modifiers.ctrl || modifiers.mac_cmd) && !modifiers.alt => {
          reload = true
        }
        _ => {}
      }
    }

    (focus_address_bar, new_tab, close_tab, reload, tab_delta)
  });

  if focus_address_bar {
    actions.push(ChromeAction::FocusAddressBar);
  }
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

  if !app.chrome.address_bar_has_focus && !ctx.wants_keyboard_input() {
    let (back, forward) = ctx.input(|i| {
      // Like the Ctrl/Cmd shortcuts above, use the key event's modifier snapshot instead of
      // `i.modifiers` so this stays robust in unit tests.
      let mut back = false;
      let mut forward = false;
      for event in &i.events {
        let egui::Event::Key {
          key,
          pressed: true,
          repeat: false,
          modifiers,
        } = event
        else {
          continue;
        };
        // Guard against AltGr (often encoded as Ctrl+Alt).
        let alt_only = modifiers.alt && !(modifiers.command || modifiers.ctrl || modifiers.mac_cmd);
        if !alt_only {
          continue;
        }
        match key {
          egui::Key::ArrowLeft => back = true,
          egui::Key::ArrowRight => forward = true,
          _ => {}
        }
      }
      (back, forward)
    });
    if back {
      actions.push(ChromeAction::Back);
    }
    if forward {
      actions.push(ChromeAction::Forward);
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

#[cfg(test)]
mod tests {
  use super::{chrome_ui, ChromeAction};
  use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
  use crate::ui::messages::TabId;

  fn new_context_with_key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Context {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.events.push(egui::Event::Key {
      key,
      pressed: true,
      repeat: false,
      modifiers,
    });
    ctx.begin_frame(raw);
    ctx
  }

  #[test]
  fn ctrl_l_emits_focus_address_bar_action() {
    let mut app = BrowserAppState::new();

    // Egui's `Event::Key` carries a modifier snapshot.
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::L, modifiers);
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::FocusAddressBar)),
      "expected ChromeAction::FocusAddressBar, got {actions:?}"
    );
  }

  #[test]
  fn f5_emits_reload_action() {
    let mut app = BrowserAppState::new();

    let ctx = new_context_with_key(egui::Key::F5, Default::default());
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Reload)),
      "expected ChromeAction::Reload, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_r_emits_reload_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::R,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::Reload)),
      "expected ChromeAction::Reload, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_t_emits_new_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::T,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::NewTab)),
      "expected ChromeAction::NewTab, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_w_emits_close_tab_for_active_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::W,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTab(id) if *id == tab_id)),
      "expected ChromeAction::CloseTab({tab_id:?}), got {actions:?}"
    );
  }

  #[test]
  fn ctrl_tab_cycles_tabs_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::Tab,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ActivateTab(id) if *id == tab_b)),
      "expected ChromeAction::ActivateTab({tab_b:?}), got {actions:?}"
    );
  }

  #[test]
  fn alt_left_emits_back_action_when_not_editing() {
    let mut app = BrowserAppState::new();
    let ctx = new_context_with_key(
      egui::Key::ArrowLeft,
      egui::Modifiers {
        alt: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::Back)),
      "expected ChromeAction::Back, got {actions:?}"
    );
  }

  #[test]
  fn alt_right_emits_forward_action_when_not_editing() {
    let mut app = BrowserAppState::new();
    let ctx = new_context_with_key(
      egui::Key::ArrowRight,
      egui::Modifiers {
        alt: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::Forward)),
      "expected ChromeAction::Forward, got {actions:?}"
    );
  }

  #[test]
  fn alt_left_is_suppressed_while_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;
    let ctx = new_context_with_key(
      egui::Key::ArrowLeft,
      egui::Modifiers {
        alt: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      !actions.iter().any(|action| matches!(action, ChromeAction::Back)),
      "expected ChromeAction::Back to be suppressed, got {actions:?}"
    );
  }
}
