#![cfg(feature = "browser_ui")]

use crate::ui::browser_app::BrowserAppState;
use crate::render_control::StageHeartbeat;
use crate::ui::messages::TabId;
use crate::ui::zoom;

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
  ReopenClosedTab,
  ActivateTab(TabId),
  NavigateTo(String),
  Back,
  Forward,
  Reload,
  AddressBarFocusChanged(bool),
}

pub fn chrome_ui(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  mut favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
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
  //
  // Ctrl/Cmd+L should not steal focus from other egui text fields (e.g. devtools inputs), but we
  // still want it to re-select the URL when the address bar is already focused.
  let allow_focus_address_bar = !ctx.wants_keyboard_input() || app.chrome.address_bar_has_focus;
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum ZoomAction {
    In,
    Out,
    Reset,
  }

  let (
    focus_address_bar,
    new_tab,
    close_tab,
    reopen_closed_tab,
    reload,
    tab_delta,
    tab_number,
    zoom_action,
  ) = ctx.input(|i| {
    // Use the key event's modifier snapshot rather than `i.modifiers`: the winit integration feeds
    // modifiers via events, and using the event snapshot keeps this robust in unit tests as well.
    let mut focus_address_bar = false;
    let mut new_tab = false;
    let mut close_tab = false;
    let mut reopen_closed_tab = false;
    let mut reload = false;
    let mut tab_delta: Option<isize> = None;
    let mut tab_number: Option<u8> = None;
    let mut zoom_action: Option<ZoomAction> = None;

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
        // Many browsers support both Ctrl/Cmd+L and Ctrl/Cmd+K for focusing the address bar.
        egui::Key::L | egui::Key::K if cmd_or_ctrl && allow_focus_address_bar => {
          focus_address_bar = true
        }
        // Ctrl/Cmd+T creates a new tab; Ctrl/Cmd+Shift+T reopens the last closed tab.
        egui::Key::T if cmd_or_ctrl && modifiers.shift => reopen_closed_tab = true,
        egui::Key::T if cmd_or_ctrl => new_tab = true,
        egui::Key::W if cmd_or_ctrl => close_tab = true,
        egui::Key::R if cmd_or_ctrl => reload = true,
        // Browser-like zoom shortcuts.
        egui::Key::PlusEquals if cmd_or_ctrl => zoom_action = Some(ZoomAction::In),
        egui::Key::Minus if cmd_or_ctrl => zoom_action = Some(ZoomAction::Out),
        egui::Key::Num0 if cmd_or_ctrl => zoom_action = Some(ZoomAction::Reset),
        egui::Key::Tab if cmd_or_ctrl => {
          tab_delta = Some(if modifiers.shift { -1isize } else { 1isize })
        }
        // Ctrl/Cmd+1..9 activate tabs by index (9 = last tab), matching common browser shortcuts.
        egui::Key::Num1 if cmd_or_ctrl => tab_number = Some(1),
        egui::Key::Num2 if cmd_or_ctrl => tab_number = Some(2),
        egui::Key::Num3 if cmd_or_ctrl => tab_number = Some(3),
        egui::Key::Num4 if cmd_or_ctrl => tab_number = Some(4),
        egui::Key::Num5 if cmd_or_ctrl => tab_number = Some(5),
        egui::Key::Num6 if cmd_or_ctrl => tab_number = Some(6),
        egui::Key::Num7 if cmd_or_ctrl => tab_number = Some(7),
        egui::Key::Num8 if cmd_or_ctrl => tab_number = Some(8),
        egui::Key::Num9 if cmd_or_ctrl => tab_number = Some(9),
        // F5 is a common reload shortcut. Ignore modified F5 (Ctrl/Cmd+F5 / Alt+F5).
        egui::Key::F5 if !(modifiers.command || modifiers.ctrl || modifiers.mac_cmd) && !modifiers.alt => {
          reload = true
        }
        _ => {}
      }
    }

    (
      focus_address_bar,
      new_tab,
      close_tab,
      reopen_closed_tab,
      reload,
      tab_delta,
      tab_number,
      zoom_action,
    )
  });

  if focus_address_bar {
    actions.push(ChromeAction::FocusAddressBar);
    // Apply the focus/select changes immediately (this frame) so the address bar widget can
    // consume them when it's built below.
    app.chrome.request_focus_address_bar = true;
    app.chrome.request_select_all_address_bar = true;
  }
  if new_tab {
    actions.push(ChromeAction::NewTab);
  }
  if close_tab {
    if app.tabs.len() > 1 {
      if let Some(tab_id) = app.active_tab_id() {
        actions.push(ChromeAction::CloseTab(tab_id));
      }
    }
  }
  if reopen_closed_tab {
    actions.push(ChromeAction::ReopenClosedTab);
  }
  if reload {
    actions.push(ChromeAction::Reload);
  }
  if let Some(zoom_action) = zoom_action {
    if let Some(tab) = app.active_tab_mut() {
      tab.zoom = match zoom_action {
        ZoomAction::In => zoom::zoom_in(tab.zoom),
        ZoomAction::Out => zoom::zoom_out(tab.zoom),
        ZoomAction::Reset => zoom::zoom_reset(),
      };
    }
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
  if let Some(tab_number) = tab_number {
    let len = app.tabs.len();
    if len > 0 {
      let idx = if tab_number == 9 {
        len - 1
      } else {
        (tab_number.saturating_sub(1)) as usize
      };
      if let Some(tab) = app.tabs.get(idx) {
        actions.push(ChromeAction::ActivateTab(tab.id));
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
      let can_close_tabs = app.tabs.len() > 1;
      for tab in &app.tabs {
        let is_active = app.active_tab_id() == Some(tab.id);
        let title = tab.display_title();

        if let Some(tex_id) = favicon_for_tab(tab.id) {
          if let Some(meta) = tab.favicon_meta {
            let (w, h) = meta.size_px;
            if w > 0 && h > 0 {
              let height_points = 16.0;
              let aspect = (w as f32) / (h as f32);
              let width_points = (height_points * aspect).clamp(8.0, 32.0);
              if ui
                .add(
                  egui::Image::new((tex_id, egui::vec2(width_points, height_points)))
                    .sense(egui::Sense::click()),
                )
                .clicked()
              {
                actions.push(ChromeAction::ActivateTab(tab.id));
              }
            }
          }
        }

        if ui.selectable_label(is_active, title).clicked() {
          actions.push(ChromeAction::ActivateTab(tab.id));
        }

        if ui
          .add_enabled(can_close_tabs, egui::Button::new("×"))
          .clicked()
        {
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
      let (can_back, can_forward, loading, stage, warning, error, zoom_factor) = app
        .active_tab()
        .map(|t| {
          (
            t.can_go_back,
            t.can_go_forward,
            t.loading,
            t.stage,
            t.warning.clone(),
            t.error.clone(),
            t.zoom,
          )
        })
        .unwrap_or((false, false, false, None, None, None, zoom::DEFAULT_ZOOM));

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

      // Zoom controls (optional, but useful for discoverability and as a fallback on platforms with
      // non-US keyboard layouts).
      if ui.small_button("−").clicked() {
        if let Some(tab) = app.active_tab_mut() {
          tab.zoom = zoom::zoom_out(tab.zoom);
        }
      }
      let percent = zoom::zoom_percent(zoom_factor);
      if ui
        .small_button(format!("{percent}%"))
        .on_hover_text("Reset zoom (Ctrl/Cmd+0)")
        .clicked()
      {
        if let Some(tab) = app.active_tab_mut() {
          tab.zoom = zoom::zoom_reset();
        }
      }
      if ui.small_button("+").clicked() {
        if let Some(tab) = app.active_tab_mut() {
          tab.zoom = zoom::zoom_in(tab.zoom);
        }
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
        // Keep `address_bar_editing` and `address_bar_has_focus` in sync, and when the user leaves
        // the address bar revert any uncommitted edits back to the active tab URL (matching typical
        // browser UX).
        app.set_address_bar_editing(has_focus);
        actions.push(ChromeAction::AddressBarFocusChanged(has_focus));
      }

      // Some chrome actions (e.g. switching tabs via Ctrl/Cmd+Tab) cancel address bar editing while
      // keeping focus in the widget. Ensure we re-enter "editing" mode as soon as the user starts
      // typing, so worker navigation updates don't clobber in-progress input.
      if has_focus && response.changed() {
        app.chrome.address_bar_editing = true;
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

      if let Some(warn) = warning.as_deref().filter(|s| !s.trim().is_empty()) {
        ui.label(
          egui::RichText::new("⚠")
            .color(egui::Color32::BLACK)
            .background_color(egui::Color32::from_rgb(250, 230, 150)),
        )
        .on_hover_text(warn);
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

  fn new_context() -> egui::Context {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    ctx.begin_frame(raw);
    ctx
  }

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

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.events = events;
    ctx.begin_frame(raw);
  }

  #[test]
  fn address_bar_blur_reverts_uncommitted_text() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com".to_string()),
      true,
    );

    app.chrome.address_bar_text = "https://typed.example".to_string();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context();
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com");
    assert!(!app.chrome.address_bar_has_focus);
    assert!(!app.chrome.address_bar_editing);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::FocusAddressBar)),
      "expected ChromeAction::FocusAddressBar, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_k_emits_focus_address_bar_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::K, modifiers);
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::NewTab)),
      "expected ChromeAction::NewTab, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_shift_t_emits_reopen_closed_tab_action() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::T,
      egui::Modifiers {
        command: true,
        shift: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ReopenClosedTab)),
      "expected ChromeAction::ReopenClosedTab, got {actions:?}"
    );
    assert!(
      !actions.iter().any(|action| matches!(action, ChromeAction::NewTab)),
      "expected Ctrl/Cmd+Shift+T not to emit NewTab, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_w_emits_close_tab_for_active_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::W,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTab(id) if *id == tab_a)),
      "expected ChromeAction::CloseTab({tab_a:?}), got {actions:?}"
    );
  }

  #[test]
  fn ctrl_w_is_noop_when_only_one_tab_exists() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

    let ctx = new_context_with_key(
      egui::Key::W,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions.iter().any(|action| matches!(action, ChromeAction::CloseTab(_))),
      "expected no CloseTab action when only one tab exists, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_plus_zooms_in_active_tab() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

    let ctx = new_context_with_key(
      egui::Key::PlusEquals,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    let zoom = app.active_tab().unwrap().zoom;
    assert!(zoom > 1.0, "expected zoom to increase, got {zoom}");
  }

  #[test]
  fn ctrl_minus_zooms_out_active_tab() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

    let ctx = new_context_with_key(
      egui::Key::Minus,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();

    let zoom = app.active_tab().unwrap().zoom;
    assert!(zoom < 1.0, "expected zoom to decrease, got {zoom}");
  }

  #[test]
  fn ctrl_0_resets_zoom() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

    // First zoom in.
    let ctx = new_context_with_key(
      egui::Key::PlusEquals,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();
    assert!(app.active_tab().unwrap().zoom > 1.0);

    // Then reset.
    let ctx = new_context_with_key(
      egui::Key::Num0,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app);
    let _ = ctx.end_frame();
    assert!(
      (app.active_tab().unwrap().zoom - crate::ui::zoom::DEFAULT_ZOOM).abs() < f32::EPSILON
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ActivateTab(id) if *id == tab_b)),
      "expected ChromeAction::ActivateTab({tab_b:?}), got {actions:?}"
    );
  }

  #[test]
  fn ctrl_1_activates_first_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), true);
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::Num1,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ActivateTab(id) if *id == tab_a)),
      "expected ChromeAction::ActivateTab({tab_a:?}), got {actions:?}"
    );
  }

  #[test]
  fn ctrl_9_activates_last_tab() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);

    let ctx = new_context_with_key(
      egui::Key::Num9,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions.iter().any(|action| matches!(action, ChromeAction::Back)),
      "expected ChromeAction::Back to be suppressed, got {actions:?}"
    );
  }

  #[test]
  fn address_bar_typing_sets_editing_even_when_focus_does_not_change() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "https://a.example/".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "https://b.example/".to_string()), false);

    let ctx = egui::Context::default();

    // Frame 1: focus the address bar (simulating Ctrl/Cmd+L).
    app.chrome.request_focus_address_bar = true;
    app.chrome.request_select_all_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: let egui apply the focus request.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus, "expected address bar to be focused");

    // Switching tabs cancels editing but (in a real UI) focus may remain in the address bar.
    assert!(app.set_active_tab(tab_b));
    assert!(app.chrome.address_bar_has_focus);
    assert!(
      !app.chrome.address_bar_editing,
      "expected tab switch to cancel address bar editing"
    );

    // Now type a character while focus stays in the address bar. This should re-enable the
    // `address_bar_editing` flag so worker updates don't clobber the typed text.
    begin_frame(
      &ctx,
      vec![egui::Event::Text("x".to_string())],
    );
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_editing);
  }
}
