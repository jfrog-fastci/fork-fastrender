#![cfg(feature = "browser_ui")]

use crate::render_control::StageHeartbeat;
use crate::ui::browser_app::BrowserAppState;
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::shortcuts::{map_shortcut, Key, KeyEvent, Modifiers, ShortcutAction};
use crate::ui::zoom;
use crate::ui::{icon_button, icon_tinted, spinner, BrowserIcon};
use url::Url;

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

fn egui_modifiers_to_shortcuts_modifiers(modifiers: egui::Modifiers) -> Modifiers {
  // Egui exposes a cross-platform `command` flag (Cmd on macOS, Ctrl elsewhere). For our canonical
  // shortcut mapping we want explicit `ctrl` + `meta`.
  let (ctrl, meta) = if cfg!(target_os = "macos") {
    // On macOS, `command` and `mac_cmd` both represent Cmd. Allow Ctrl as well (useful for
    // Ctrl+Tab-style navigation in some apps, and matches the previous chrome implementation).
    (modifiers.ctrl, modifiers.command || modifiers.mac_cmd)
  } else {
    // On non-mac platforms, `command` is effectively Ctrl. Egui does not currently expose the
    // Windows/Super key separately, so leave `meta` false here.
    (modifiers.command || modifiers.ctrl, false)
  };

  Modifiers {
    ctrl,
    shift: modifiers.shift,
    alt: modifiers.alt,
    meta,
  }
}

fn egui_key_to_shortcuts_key(key: egui::Key) -> Option<Key> {
  Some(match key {
    egui::Key::A => Key::A,
    egui::Key::C => Key::C,
    egui::Key::D => Key::D,
    egui::Key::K => Key::K,
    egui::Key::L => Key::L,
    egui::Key::R => Key::R,
    egui::Key::T => Key::T,
    egui::Key::V => Key::V,
    egui::Key::W => Key::W,
    egui::Key::X => Key::X,
    egui::Key::Tab => Key::Tab,
    egui::Key::ArrowLeft => Key::Left,
    egui::Key::ArrowRight => Key::Right,
    egui::Key::Num0 => Key::Num0,
    egui::Key::Num1 => Key::Num1,
    egui::Key::Num2 => Key::Num2,
    egui::Key::Num3 => Key::Num3,
    egui::Key::Num4 => Key::Num4,
    egui::Key::Num5 => Key::Num5,
    egui::Key::Num6 => Key::Num6,
    egui::Key::Num7 => Key::Num7,
    egui::Key::Num8 => Key::Num8,
    egui::Key::Num9 => Key::Num9,
    egui::Key::F4 => Key::F4,
    egui::Key::F5 => Key::F5,
    egui::Key::F6 => Key::F6,
    egui::Key::PlusEquals => Key::Equals,
    egui::Key::Minus => Key::Minus,
    egui::Key::PageUp => Key::PageUp,
    egui::Key::PageDown => Key::PageDown,
    egui::Key::Space => Key::Space,
    egui::Key::Home => Key::Home,
    egui::Key::End => Key::End,
    _ => return None,
  })
}

fn with_alpha(color: egui::Color32, alpha: f32) -> egui::Color32 {
  let [r, g, b, a] = color.to_array();
  let a = ((a as f32) * alpha).round().clamp(0.0, 255.0) as u8;
  egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

pub fn chrome_ui(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  mut favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
  let mut actions = Vec::new();
  let motion = UiMotion::from_env();

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
  let allow_history_navigation = if cfg!(target_os = "macos") {
    // On macOS, the canonical back/forward shortcuts are Cmd+[ / Cmd+]. These do not interfere with
    // word-wise cursor movement in text fields, so allow history navigation even while the address
    // bar is focused.
    !ctx.wants_keyboard_input() || app.chrome.address_bar_has_focus
  } else {
    // On other platforms, back/forward is typically Alt+Left/Right, which is also used for
    // word-wise cursor movement in some text fields. Suppress history navigation while the address
    // bar is focused to avoid stealing those editing gestures.
    !ctx.wants_keyboard_input() && !app.chrome.address_bar_has_focus
  };
  let (
    focus_address_bar,
    new_tab,
    close_tab,
    reopen_closed_tab,
    reload,
    tab_delta,
    tab_number,
    back,
    forward,
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
    let mut back = false;
    let mut forward = false;
    let mut zoom_action: Option<ShortcutAction> = None;

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

      let Some(shortcut_key) = egui_key_to_shortcuts_key(*key) else {
        continue;
      };
      let shortcut_modifiers = egui_modifiers_to_shortcuts_modifiers(*modifiers);
      let Some(action) = map_shortcut(KeyEvent::new(shortcut_key, shortcut_modifiers)) else {
        continue;
      };

      match action {
        ShortcutAction::FocusAddressBar if allow_focus_address_bar => {
          focus_address_bar = true;
        }
        ShortcutAction::NewTab => new_tab = true,
        ShortcutAction::CloseTab => close_tab = true,
        ShortcutAction::ReopenClosedTab => reopen_closed_tab = true,
        ShortcutAction::Reload => reload = true,
        ShortcutAction::NextTab => tab_delta = Some(1),
        ShortcutAction::PrevTab => tab_delta = Some(-1),
        ShortcutAction::ActivateTabNumber(n) => tab_number = Some(n),
        ShortcutAction::Back if allow_history_navigation => back = true,
        ShortcutAction::Forward if allow_history_navigation => forward = true,
        ShortcutAction::ZoomIn | ShortcutAction::ZoomOut | ShortcutAction::ZoomReset => {
          zoom_action = Some(action);
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
      back,
      forward,
      zoom_action,
    )
  });

  // -----------------------------------------------------------------------------
  // Ctrl/Cmd+mouse wheel zoom
  // -----------------------------------------------------------------------------
  //
  // Many browsers map Ctrl/Cmd+wheel to page zoom. Handle this at the chrome level so it works
  // regardless of where the cursor is (page area or chrome), and so windowed front-ends can filter
  // these wheel events out of the page scroll path.
  //
  // Note: we intentionally keep the mapping simple: any positive wheel delta zooms in, any negative
  // delta zooms out, and a frame may apply multiple steps if multiple wheel events are queued.
  ctx.input(|i| {
    for event in &i.events {
      let egui::Event::MouseWheel {
        delta, modifiers, ..
      } = event
      else {
        continue;
      };
      if !modifiers.command {
        continue;
      }
      let mut wheel = delta.y;
      if wheel == 0.0 {
        wheel = delta.x;
      }
      if !wheel.is_finite() || wheel == 0.0 {
        continue;
      }
      if let Some(tab) = app.active_tab_mut() {
        tab.zoom = if wheel > 0.0 {
          zoom::zoom_in(tab.zoom)
        } else {
          zoom::zoom_out(tab.zoom)
        };
      }
    }
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
        ShortcutAction::ZoomIn => zoom::zoom_in(tab.zoom),
        ShortcutAction::ZoomOut => zoom::zoom_out(tab.zoom),
        ShortcutAction::ZoomReset => zoom::zoom_reset(),
        _ => tab.zoom,
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

  if back {
    actions.push(ChromeAction::Back);
  }
  if forward {
    actions.push(ChromeAction::Forward);
  }
  egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
    // Tabs row.
    ui.horizontal_wrapped(|ui| {
      let can_close_tabs = app.tabs.len() > 1;
      for tab in &app.tabs {
        let is_active = app.active_tab_id() == Some(tab.id);
        let title = tab.display_title();

        let tab_anim_id = ui.make_persistent_id(("tab_anim", tab.id));
        let inner = ui.horizontal(|ui| {
          if let Some(tex_id) = favicon_for_tab(tab.id) {
            if let Some(meta) = tab.favicon_meta {
              let (w, h) = meta.size_px;
              if w > 0 && h > 0 {
                let height_points = 16.0;
                let aspect = (w as f32) / (h as f32);
                let width_points = (height_points * aspect).clamp(8.0, 32.0);
                let response = ui.add(
                  egui::Image::new((tex_id, egui::vec2(width_points, height_points)))
                    .sense(egui::Sense::click()),
                );
                if response.clicked_by(egui::PointerButton::Middle) {
                  if can_close_tabs {
                    actions.push(ChromeAction::CloseTab(tab.id));
                  }
                } else if response.clicked() {
                  actions.push(ChromeAction::ActivateTab(tab.id));
                }
              }
            }
          }

          let response = ui.selectable_label(is_active, title);
          if response.clicked_by(egui::PointerButton::Middle) {
            if can_close_tabs {
              actions.push(ChromeAction::CloseTab(tab.id));
            }
          } else if response.clicked() {
            actions.push(ChromeAction::ActivateTab(tab.id));
          }

          if icon_button(
            ui,
            BrowserIcon::CloseTab,
            "Close tab (Ctrl/Cmd+W)",
            can_close_tabs,
          )
          .clicked()
          {
            actions.push(ChromeAction::CloseTab(tab.id));
          }
        });

        // Micro-interactions: tab hover highlight + active underline.
        let hovered = inner.response.hovered();
        let hover_t = motion.animate_bool(
          ctx,
          tab_anim_id.with("hover"),
          hovered,
          motion.durations.hover_fade,
        );
        if hover_t > 0.0 {
          // We draw the hover highlight on the same layer as the tab contents, so keep the alpha
          // intentionally low to avoid obscuring text/icons.
          let max_alpha = if ui.visuals().dark_mode { 0.25 } else { 0.15 };
          let hover_fill = with_alpha(ui.visuals().widgets.hovered.bg_fill, hover_t * max_alpha);
          ui.painter().rect_filled(
            inner.response.rect.expand(2.0),
            egui::Rounding::same(4.0),
            hover_fill,
          );
        }

        let active_t = motion.animate_bool(
          ctx,
          tab_anim_id.with("underline"),
          is_active,
          motion.durations.tab_underline,
        );
        if active_t > 0.0 {
          let underline_height = 2.0;
          let underline_width = inner.response.rect.width() * active_t;
          let cx = inner.response.rect.center().x;
          let x0 = cx - underline_width * 0.5;
          let x1 = cx + underline_width * 0.5;
          let y1 = inner.response.rect.bottom();
          let y0 = y1 - underline_height;
          let rect = egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1));
          let color = with_alpha(ui.visuals().selection.stroke.color, active_t);
          ui
            .painter()
            .rect_filled(rect, egui::Rounding::same(1.0), color);
        }

        ui.separator();
      }

      if icon_button(ui, BrowserIcon::NewTab, "New tab (Ctrl/Cmd+T)", true).clicked() {
        actions.push(ChromeAction::NewTab);
      }
    });

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let (can_back, can_forward, loading, stage, load_progress, warning, error, zoom_factor) = app
        .active_tab()
        .map(|t| {
          (
            t.can_go_back,
            t.can_go_forward,
            t.loading,
            t.stage,
            t.load_progress,
            t.warning.clone(),
            t.error.clone(),
            t.zoom,
          )
        })
        .unwrap_or((false, false, false, None, None, None, None, zoom::DEFAULT_ZOOM));

      if icon_button(ui, BrowserIcon::Back, "Back", can_back).clicked() {
        actions.push(ChromeAction::Back);
      }
      if icon_button(ui, BrowserIcon::Forward, "Forward", can_forward).clicked() {
        actions.push(ChromeAction::Forward);
      }
      if icon_button(ui, BrowserIcon::Reload, "Reload (Ctrl/Cmd+R)", true).clicked() {
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

      // Security indicator (non-interactive).
      if let Some(committed) = app.active_tab().and_then(|t| t.committed_url.as_deref()) {
        if let Ok(parsed) = Url::parse(committed) {
          match parsed.scheme() {
            "https" => {
              let _ = icon_tinted(
                ui,
                BrowserIcon::LockSecure,
                ui.spacing().icon_width,
                ui.visuals().text_color(),
              )
              .on_hover_text("Secure connection (HTTPS)");
            }
            "http" => {
              let _ = icon_tinted(
                ui,
                BrowserIcon::WarningInsecure,
                ui.spacing().icon_width,
                ui.visuals().text_color(),
              )
              .on_hover_text("Not secure (HTTP)");
            }
            _ => {}
          }
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

      // Micro-interaction: address bar focus ring animation.
      let focus_t = motion.animate_bool(
        ctx,
        address_bar_id.with("focus_ring"),
        has_focus,
        motion.durations.focus_ring,
      );
      if focus_t > 0.0 {
        let ring_color = with_alpha(ui.visuals().selection.stroke.color, focus_t);
        let ring_width = 1.0 + focus_t;
        let ring_rect = response.rect.expand(1.0 + focus_t);
        ui.painter().rect_stroke(
          ring_rect,
          egui::Rounding::same(4.0),
          egui::Stroke::new(ring_width, ring_color),
        );
      }

      // Micro-interaction: loading progress indicator (fade in/out).
      let loading_t = motion.animate_bool(
        ctx,
        address_bar_id.with("loading_progress"),
        loading,
        motion.durations.progress_fade,
      );
      if loading_t > 0.0 {
        let bar_h = 2.0;
        let progress = load_progress
          .filter(|p| p.is_finite())
          .map(|p| p.clamp(0.0, 1.0))
          .unwrap_or(0.0);
        // Ensure the bar appears quickly on navigation start even before the first stage heartbeat.
        let progress = if loading { progress.max(0.02) } else { progress };
        let x1 = response.rect.left() + response.rect.width() * progress;
        let rect = egui::Rect::from_min_max(
          egui::pos2(response.rect.left(), response.rect.bottom() - bar_h),
          egui::pos2(x1, response.rect.bottom()),
        );
        let color = with_alpha(ui.visuals().selection.stroke.color, loading_t);
        if rect.width() > 0.0 {
          ui
            .painter()
            .rect_filled(rect, egui::Rounding::same(1.0), color);
        }
      }

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
        actions.push(ChromeAction::NavigateTo(
          app.chrome.address_bar_text.clone(),
        ));
        response.surrender_focus();
      }

      if loading {
        let _ = spinner(ui, ui.spacing().icon_width);
        let stage = stage.filter(|s| *s != StageHeartbeat::Done);
        match stage {
          Some(stage) => {
            ui.label(egui::RichText::new(format!("Loading… {}", stage.as_str())).small())
          }
          None => ui.label(egui::RichText::new("Loading…").small()),
        };
      }

      if let Some(warn) = warning.as_deref().filter(|s| !s.trim().is_empty()) {
        let resp = egui::Frame::none()
          .fill(egui::Color32::from_rgb(250, 230, 150))
          .rounding(egui::Rounding::same(3.0))
          .inner_margin(egui::Margin::same(2.0))
          .show(ui, |ui| {
            let _ = icon_tinted(ui, BrowserIcon::WarningInsecure, ui.spacing().icon_width, egui::Color32::BLACK);
          })
          .response;
        let _ = resp.on_hover_text(warn);
      }

      if let Some(err) = error.as_deref().filter(|s| !s.trim().is_empty()) {
        let resp = egui::Frame::none()
          .fill(egui::Color32::from_rgb(160, 0, 0))
          .rounding(egui::Rounding::same(3.0))
          .inner_margin(egui::Margin::same(2.0))
          .show(ui, |ui| {
            let _ = icon_tinted(ui, BrowserIcon::Error, ui.spacing().icon_width, egui::Color32::WHITE);
          })
          .response;
        let _ = resp.on_hover_text(err);
      }
    });
  });

  // ---------------------------------------------------------------------------
  // Hovered-link status
  // ---------------------------------------------------------------------------
  //
  // Canonical UX: show hovered URLs in a dedicated bottom status bar (browser-like).
  //
  // We intentionally avoid also rendering an in-page hover overlay: it duplicates the URL and can
  // look messy on top of page content.
  //
  // Always reserve space for the status bar so showing/hiding the hovered URL doesn't change the
  // page viewport size (which would trigger needless repaints and can cause hover flicker).
  const STATUS_BAR_HEIGHT: f32 = 24.0;
  let hovered_url = app
    .active_tab()
    .and_then(|t| t.hovered_url.as_deref())
    .map(str::trim)
    .filter(|s| !s.is_empty());

  egui::TopBottomPanel::bottom("status_bar")
    .resizable(false)
    .default_height(STATUS_BAR_HEIGHT)
    .min_height(STATUS_BAR_HEIGHT)
    .max_height(STATUS_BAR_HEIGHT)
    .show(ctx, |ui| {
      ui.horizontal(|ui| {
        ui.add_space(4.0);

        if let Some(url) = hovered_url {
          let visuals = ui.visuals();
          let frame = egui::Frame::none()
            .fill(visuals.widgets.inactive.bg_fill)
            .stroke(visuals.widgets.inactive.bg_stroke)
            .rounding(egui::Rounding::same(4.0))
            .inner_margin(egui::Margin::symmetric(8.0, 2.0));
          frame.show(ui, |ui| {
            // Use a read-only `TextEdit` so the hovered URL is selectable/copyable.
            let mut url_owned = url.to_string();
            let max_width = ui.available_width().min(600.0);
            let desired_width = ui
              .fonts(|f| {
                let font_id = egui::TextStyle::Small.resolve(ui.style());
                f.layout_no_wrap(url_owned.clone(), font_id, ui.visuals().text_color())
              })
              .size()
              .x
              .min(max_width);
            ui.add(
              egui::TextEdit::singleline(&mut url_owned)
                .id(ui.make_persistent_id("hovered_url_status_text"))
                .font(egui::TextStyle::Small)
                .desired_width(desired_width)
                .interactive(false)
                .frame(false),
            );
          });
        } else {
          // Preserve the bar height even when no URL is displayed.
          ui.add(egui::Label::new(egui::RichText::new(" ").small()));
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
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
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
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
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
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn middle_click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    vec![
      egui::Event::PointerMoved(pos),
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Middle,
        pressed: true,
        modifiers: egui::Modifiers::default(),
      },
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Middle,
        pressed: false,
        modifiers: egui::Modifiers::default(),
      },
    ]
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

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn alt_d_emits_focus_address_bar_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      alt: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::D, modifiers);
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
  fn f6_emits_focus_address_bar_action() {
    let mut app = BrowserAppState::new();
    let ctx = new_context_with_key(egui::Key::F6, Default::default());
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
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Reload)),
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
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NewTab)),
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
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ReopenClosedTab)),
      "expected ChromeAction::ReopenClosedTab, got {actions:?}"
    );
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NewTab)),
      "expected Ctrl/Cmd+Shift+T not to emit NewTab, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_w_emits_close_tab_for_active_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );
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
  fn ctrl_f4_emits_close_tab_for_active_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::F4,
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
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

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
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTab(_))),
      "expected no CloseTab action when only one tab exists, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_plus_zooms_in_active_tab() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = new_context_with_key(
      egui::Key::PlusEquals,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    let zoom = app.active_tab().unwrap().zoom;
    assert!(zoom > 1.0, "expected zoom to increase, got {zoom}");
  }

  #[test]
  fn ctrl_minus_zooms_out_active_tab() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = new_context_with_key(
      egui::Key::Minus,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    let zoom = app.active_tab().unwrap().zoom;
    assert!(zoom < 1.0, "expected zoom to decrease, got {zoom}");
  }

  #[test]
  fn ctrl_0_resets_zoom() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    // First zoom in.
    let ctx = new_context_with_key(
      egui::Key::PlusEquals,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
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
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();
    assert!((app.active_tab().unwrap().zoom - crate::ui::zoom::DEFAULT_ZOOM).abs() < f32::EPSILON);
  }

  #[test]
  fn ctrl_tab_cycles_tabs_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );
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
  fn ctrl_page_down_cycles_tabs_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::PageDown,
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
  fn ctrl_page_up_cycles_tabs_backward_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      true,
    );
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::PageUp,
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
  fn ctrl_shift_tab_cycles_tabs_backward_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      true,
    );
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::Tab,
      egui::Modifiers {
        command: true,
        shift: true,
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
  fn ctrl_1_activates_first_tab_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      true,
    );
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
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );

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
  fn ctrl_alt_shortcuts_are_ignored_to_avoid_altgr() {
    let mut app = BrowserAppState::new();

    let ctx = new_context_with_key(
      egui::Key::T,
      egui::Modifiers {
        command: true,
        alt: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NewTab)),
      "expected Ctrl+Alt+T to be ignored (AltGr guard), got {actions:?}"
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
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Back)),
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
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Forward)),
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
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Back)),
      "expected ChromeAction::Back to be suppressed, got {actions:?}"
    );
  }

  #[test]
  fn address_bar_typing_sets_editing_even_when_focus_does_not_change() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://b.example/".to_string()),
      false,
    );

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

    assert!(
      app.chrome.address_bar_has_focus,
      "expected address bar to be focused"
    );

    // Switching tabs cancels editing but (in a real UI) focus may remain in the address bar.
    assert!(app.set_active_tab(tab_b));
    assert!(app.chrome.address_bar_has_focus);
    assert!(
      !app.chrome.address_bar_editing,
      "expected tab switch to cancel address bar editing"
    );

    // Now type a character while focus stays in the address bar. This should re-enable the
    // `address_bar_editing` flag so worker updates don't clobber the typed text.
    begin_frame(&ctx, vec![egui::Event::Text("x".to_string())]);
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_editing);
  }

  #[test]
  fn middle_click_tab_label_closes_tab_when_multiple_tabs_exist() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );

    // The first tab label should appear near the top-left of the chrome panel.
    let ctx = egui::Context::default();
    begin_frame(&ctx, middle_click_at(egui::pos2(10.0, 10.0)));
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTab(id) if *id == tab_a)),
      "expected middle-click to close tab {tab_a:?}, got {actions:?}"
    );
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ActivateTab(id) if *id == tab_a)),
      "expected middle-click not to activate the tab it closes, got {actions:?}"
    );
  }

  #[test]
  fn middle_click_tab_label_is_noop_when_only_one_tab_exists() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    begin_frame(&ctx, middle_click_at(egui::pos2(10.0, 10.0)));
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTab(_))),
      "expected middle-click not to close the last remaining tab, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_wheel_zooms_active_tab() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();

    // Wheel up with Ctrl/Cmd => zoom in.
    begin_frame(
      &ctx,
      vec![egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 10.0),
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      }],
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    let zoom_after_in = app.active_tab().unwrap().zoom;
    assert!(zoom_after_in > crate::ui::zoom::DEFAULT_ZOOM);

    // Wheel down with Ctrl/Cmd => zoom out.
    begin_frame(
      &ctx,
      vec![egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -10.0),
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      }],
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    let zoom_after_out = app.active_tab().unwrap().zoom;
    assert!(zoom_after_out < zoom_after_in, "expected zoom to decrease");
  }
}
