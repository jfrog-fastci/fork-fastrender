#![cfg(feature = "browser_ui")]

use crate::render_control::StageHeartbeat;
use crate::ui::browser_app::BrowserAppState;
use crate::ui::load_progress::{load_progress_indicator, LoadProgressIndicator};
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::security_indicator;
use crate::ui::shortcuts::{map_shortcut, Key, KeyEvent, Modifiers, ShortcutAction};
use crate::ui::url_display;
use crate::ui::zoom;
use crate::ui::{icon_button, icon_tinted, spinner, BrowserIcon};

const ADDRESS_BAR_DISPLAY_MAX_CHARS: usize = 80;
const COMPACT_MODE_THRESHOLD_PX: f32 = 640.0;

mod tab_strip;

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
    egui::Key::N => Key::N,
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
    actions.extend(tab_strip::tab_strip_ui(ui, app, &mut favicon_for_tab));

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let is_compact = ui.available_width() < COMPACT_MODE_THRESHOLD_PX;
      let (can_back, can_forward, loading, stage, load_progress, warning, error, zoom_factor) = app
        .active_tab()
        .map(|t| {
          (
            t.can_go_back,
            t.can_go_forward,
            t.loading,
            t.load_stage,
            t.load_progress,
            t.warning.clone(),
            t.error.clone(),
            t.zoom,
          )
        })
        .unwrap_or((false, false, false, None, None, None, None, zoom::DEFAULT_ZOOM));

      if icon_button(ui, BrowserIcon::Back, "Back (Alt+Left)", can_back).clicked() {
        actions.push(ChromeAction::Back);
      }
      if icon_button(ui, BrowserIcon::Forward, "Forward (Alt+Right)", can_forward).clicked() {
        actions.push(ChromeAction::Forward);
      }
      if icon_button(ui, BrowserIcon::Reload, "Reload (Ctrl/Cmd+R)", true).clicked() {
        actions.push(ChromeAction::Reload);
      }

      // Zoom controls (optional, but useful for discoverability and as a fallback on platforms with
      // non-US keyboard layouts).
      if !is_compact {
        if icon_button(ui, BrowserIcon::ZoomOut, "Zoom out (Ctrl/Cmd+-)", true).clicked() {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_out(tab.zoom);
          }
        }
        let percent = zoom::zoom_percent(zoom_factor);
        if ui
          .button(format!("{percent}%"))
          .on_hover_text("Reset zoom (Ctrl/Cmd+0)")
          .clicked()
        {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_reset();
          }
        }
        if icon_button(ui, BrowserIcon::ZoomIn, "Zoom in (Ctrl/Cmd++)", true).clicked() {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_in(tab.zoom);
          }
        }
      }

      // ---------------------------------------------------------------------------
      // Address bar (pill + truncation + security indicator)
      // ---------------------------------------------------------------------------
      let address_bar_id = ui.make_persistent_id("address_bar");
      let egui_focus = ctx.memory(|mem| mem.has_focus(address_bar_id));
      let show_text_edit =
        egui_focus || app.chrome.address_bar_has_focus || app.chrome.request_focus_address_bar;

      // Derive the URL for display/indicator from the active tab (not from in-progress address bar
      // edits).
      let active_url = app
        .active_tab()
        .and_then(|t| t.committed_url.as_deref().or_else(|| t.current_url()))
        .unwrap_or("")
        .to_string();
      let indicator = security_indicator::indicator_for_url(&active_url);

      let stage = stage.filter(|s| *s != StageHeartbeat::Done);
      let loading_text = match stage {
        Some(stage) => format!("Loading… {}", stage.as_str()),
        None => "Loading…".to_string(),
      };

      let bar_height = ui.spacing().interact_size.y;
      let (bar_rect, bar_response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), bar_height),
        if show_text_edit {
          egui::Sense::hover()
        } else {
          egui::Sense::click()
        },
      );

      let bar_rounding = egui::Rounding::same(bar_rect.height() / 2.0);
      ui.painter().rect_filled(bar_rect, bar_rounding, ui.visuals().widgets.inactive.bg_fill);

      // Build the contents inside an inset rect to get pill-like padding.
      let pad = ui.spacing().button_padding;
      let inner_rect = bar_rect.shrink2(egui::vec2(pad.x.max(6.0), pad.y.max(4.0)));
      let mut text_edit_response: Option<egui::Response> = None;
      ui.allocate_ui_at_rect(inner_rect, |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        // Right-to-left layout ensures the URL text doesn't consume the entire width before we get a
        // chance to place status indicators on the right edge.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          if let Some(err) = error.as_deref().filter(|s| !s.trim().is_empty()) {
            let err_fg = ui.visuals().error_fg_color;
            let err_bg =
              egui::Color32::from_rgba_unmultiplied(err_fg.r(), err_fg.g(), err_fg.b(), 40);
            let resp = egui::Frame::none()
              .fill(err_bg)
              .rounding(egui::Rounding::same(3.0))
              .inner_margin(egui::Margin::same(2.0))
              .show(ui, |ui| {
                let _ = icon_tinted(ui, BrowserIcon::Error, ui.spacing().icon_width, err_fg);
              })
              .response;
            let _ = resp.on_hover_text(err);
          }

          if let Some(warn) = warning.as_deref().filter(|s| !s.trim().is_empty()) {
            let warn_fg = ui.visuals().warn_fg_color;
            let warn_bg =
              egui::Color32::from_rgba_unmultiplied(warn_fg.r(), warn_fg.g(), warn_fg.b(), 40);
            let resp = egui::Frame::none()
              .fill(warn_bg)
              .rounding(egui::Rounding::same(3.0))
              .inner_margin(egui::Margin::same(2.0))
              .show(ui, |ui| {
                let _ = icon_tinted(
                  ui,
                  BrowserIcon::WarningInsecure,
                  ui.spacing().icon_width,
                  warn_fg,
                );
              })
              .response;
            let _ = resp.on_hover_text(warn);
          }

          if loading {
            let _ = spinner(ui, ui.spacing().icon_width).on_hover_text(loading_text.clone());
            if !is_compact {
              let _ = ui
                .add(
                  egui::Label::new(egui::RichText::new(loading_text.clone()).small())
                    .wrap(false)
                    .truncate(true),
                )
                .on_hover_text(loading_text.clone());
            }
          }

          if show_text_edit {
            let response = ui.add(
              egui::TextEdit::singleline(&mut app.chrome.address_bar_text)
                .id(address_bar_id)
                .desired_width(f32::INFINITY)
                .hint_text("Enter URL…")
                .frame(false),
            );
            text_edit_response = Some(response);
          } else {
            let display = if active_url.trim().is_empty() {
              egui::RichText::new("Enter URL…").color(ui.visuals().weak_text_color())
            } else {
              egui::RichText::new(url_display::truncate_url_middle(
                &active_url,
                ADDRESS_BAR_DISPLAY_MAX_CHARS,
              ))
            };
            ui.add(egui::Label::new(display).wrap(false).truncate(true));
          }

          match indicator {
            security_indicator::SecurityIndicator::Secure => {
              let _ = icon_tinted(
                ui,
                BrowserIcon::LockSecure,
                ui.spacing().icon_width,
                ui.visuals().text_color(),
              )
              .on_hover_text(indicator.tooltip());
            }
            security_indicator::SecurityIndicator::Insecure => {
              let _ = icon_tinted(
                ui,
                BrowserIcon::WarningInsecure,
                ui.spacing().icon_width,
                ui.visuals().text_color(),
              )
              .on_hover_text(indicator.tooltip());
            }
            security_indicator::SecurityIndicator::Neutral => {
              let _ = ui
                .label(egui::RichText::new(indicator.icon()).color(ui.visuals().weak_text_color()))
                .on_hover_text(indicator.tooltip());
            }
          }
        });
      });

      // Border stroke for the pill.
      let border_stroke = if bar_response.hovered() {
        ui.visuals().widgets.hovered.bg_stroke
      } else {
        ui.visuals().widgets.inactive.bg_stroke
      };
      ui.painter().rect_stroke(bar_rect, bar_rounding, border_stroke);

      let has_focus = text_edit_response.as_ref().is_some_and(|r| r.has_focus());

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
        let ring_rect = bar_rect.expand(1.0 + focus_t);
        let ring_rounding = egui::Rounding::same(ring_rect.height() / 2.0);
        ui.painter().rect_stroke(
          ring_rect,
          ring_rounding,
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
        let indicator = load_progress_indicator(loading, load_progress);
        let bar_h = 2.0;
        let track_rect = egui::Rect::from_min_max(
          egui::pos2(bar_rect.left(), bar_rect.bottom() - bar_h),
          egui::pos2(bar_rect.right(), bar_rect.bottom()),
        );
        let color = with_alpha(ui.visuals().selection.stroke.color, loading_t);

        match indicator {
          Some(LoadProgressIndicator::Determinate { progress }) => {
            let w = track_rect.width() * progress;
            let rect = egui::Rect::from_min_max(
              track_rect.min,
              egui::pos2((track_rect.left() + w).min(track_rect.right()), track_rect.bottom()),
            );
            ui
              .painter()
              .rect_filled(rect, egui::Rounding::same(1.0), color);
          }
          Some(LoadProgressIndicator::Indeterminate) => {
            // Keep repainting so the indeterminate segment animates smoothly even when the worker
            // isn't emitting progress heartbeats.
            ctx.request_repaint();
            let time = ctx.input(|i| i.time) as f32;
            let phase = (time * 1.2).fract();
            let seg_w = (track_rect.width() * 0.25)
              .clamp(16.0, 120.0)
              .min(track_rect.width());
            let travel = (track_rect.width() - seg_w).max(0.0);
            let x0 = track_rect.left() + travel * phase;
            let seg_rect = egui::Rect::from_min_max(
              egui::pos2(x0, track_rect.top()),
              egui::pos2(x0 + seg_w, track_rect.bottom()),
            );
            ui
              .painter()
              .with_clip_rect(track_rect)
              .rect_filled(seg_rect, egui::Rounding::same(1.0), color);
          }
          None => {
            // Fade-out path: we no longer have a progress value, but keep the line around briefly
            // so it doesn't "pop" out of existence.
            ui
              .painter()
              .rect_filled(track_rect, egui::Rounding::same(1.0), color);
          }
        }
      }

      // Display mode: click-to-focus and show the full URL on hover.
      if !show_text_edit {
        let bar_response = if active_url.trim().is_empty() {
          bar_response.on_hover_text("Enter URL…")
        } else {
          bar_response.on_hover_text(active_url.clone())
        };
        if bar_response.clicked() {
          app.chrome.request_focus_address_bar = true;
          app.chrome.request_select_all_address_bar = true;
        }
      }

      if let Some(response) = text_edit_response {
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
  let (hovered_url, status_loading, status_stage, status_zoom) = app
    .active_tab()
    .map(|t| {
      (
        t.hovered_url
          .as_deref()
          .map(str::trim)
          .filter(|s| !s.is_empty()),
        t.loading,
        t.stage,
        t.zoom,
      )
    })
    .unwrap_or((None, false, None, zoom::DEFAULT_ZOOM));

  let loading_text = if status_loading {
    let stage = status_stage.filter(|s| *s != StageHeartbeat::Done);
    match stage {
      Some(stage) => Some(format!("Loading… {}", stage.as_str())),
      None => Some("Loading…".to_string()),
    }
  } else {
    None
  };

  let zoom_text = format!("{}%", zoom::zoom_percent(status_zoom));

  egui::TopBottomPanel::bottom("status_bar")
    .resizable(false)
    .default_height(STATUS_BAR_HEIGHT)
    .min_height(STATUS_BAR_HEIGHT)
    .max_height(STATUS_BAR_HEIGHT)
    .show(ctx, |ui| {
      // Use right-to-left layout so we can add right-side fields (zoom/loading) and then allocate
      // the remaining space to the hovered URL preview, which will elide when it doesn't fit.
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        // Right: zoom level.
        ui.add(
          egui::Label::new(egui::RichText::new(&zoom_text).small()).wrap(false),
        );

        // Right (optional): loading stage/progress.
        if let Some(loading_text) = loading_text.as_deref() {
          ui.add_space(8.0);
          ui.add(
            egui::Label::new(egui::RichText::new(loading_text).small()).wrap(false),
          );
        }

        ui.add_space(8.0);

        // Left: hovered URL preview.
        ui.allocate_ui_with_layout(
          egui::vec2(ui.available_width(), ui.available_height()),
          egui::Layout::left_to_right(egui::Align::Center),
          |ui| {
            ui.add_space(4.0);

            if let Some(url) = hovered_url {
              let visuals = ui.visuals();
              let frame = egui::Frame::none()
                .fill(visuals.widgets.inactive.bg_fill)
                .stroke(visuals.widgets.inactive.bg_stroke)
                .rounding(egui::Rounding::same(
                  (visuals.widgets.inactive.rounding.nw * 0.6).clamp(3.0, 6.0),
                ))
                .inner_margin({
                  let pad = ui.spacing().button_padding;
                  egui::Margin::symmetric(pad.x, pad.y * 0.4)
                });
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
          },
        );
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

  fn begin_frame_with_screen_size(
    ctx: &egui::Context,
    screen_size: egui::Vec2,
    events: Vec<egui::Event>,
  ) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      screen_size,
    ));
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

  fn collect_text_shapes(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
    match shape {
      egui::Shape::Text(t) => {
        out.push((t.galley.text().to_string(), t.pos));
      }
      egui::Shape::Vec(shapes) => {
        for s in shapes {
          collect_text_shapes(s, out);
        }
      }
      _ => {}
    }
  }

  fn status_bar_texts(output: &egui::FullOutput) -> Vec<String> {
    let mut texts = Vec::new();
    for clipped in &output.shapes {
      collect_text_shapes(&clipped.shape, &mut texts);
    }

    // `new_context` uses an 800x600 screen rect, and the status bar panel is anchored at the
    // bottom. Filter by Y position to avoid matching the zoom percent button in the top chrome.
    texts
      .into_iter()
      .filter(|(_text, pos)| pos.y > 560.0)
      .map(|(text, _pos)| text)
      .collect()
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
  fn status_bar_shows_hovered_url() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);
    app.active_tab_mut().unwrap().hovered_url = Some("https://example.com/".to_string());

    let ctx = new_context();
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let output = ctx.end_frame();

    let texts = status_bar_texts(&output);
    assert!(
      texts.iter().any(|t| t.contains("https://example.com/")),
      "expected hovered URL in status bar texts, got {texts:?}"
    );
  }

  #[test]
  fn status_bar_shows_zoom_percent_when_non_default() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);
    app.active_tab_mut().unwrap().zoom = crate::ui::zoom::zoom_in(crate::ui::zoom::DEFAULT_ZOOM);
    let expected = format!(
      "{}%",
      crate::ui::zoom::zoom_percent(app.active_tab().unwrap().zoom)
    );

    let ctx = new_context();
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let output = ctx.end_frame();

    let texts = status_bar_texts(&output);
    assert!(
      texts.iter().any(|t| t.contains(&expected)),
      "expected zoom percent {expected:?} in status bar texts, got {texts:?}"
    );
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

    let ctx = egui::Context::default();

    // Frame 1: warm up layout (some egui widgets adjust their final size after their first frame).
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
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

    // Frame 1: warm up layout.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
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
  fn tab_strip_is_single_row_and_constant_height_at_narrow_widths() {
    let mut app = BrowserAppState::new();
    // Enough tabs that the previous `horizontal_wrapped` implementation would create multiple rows
    // at narrow widths.
    for i in 0..12 {
      let tab_id = TabId((i + 1) as u64);
      app.push_tab(
        BrowserTabState::new(tab_id, format!("https://example.com/{i}")),
        i == 0,
      );
    }

    // Wide frame.
    let ctx_wide = egui::Context::default();
    begin_frame_with_screen_size(&ctx_wide, egui::vec2(800.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx_wide, &mut app, |_| None);
    let (wide_strip, wide_tabs) =
      super::tab_strip::load_test_layout(&ctx_wide).expect("missing tab strip layout metrics");
    let _ = ctx_wide.end_frame();

    // Narrow frame.
    let ctx_narrow = egui::Context::default();
    begin_frame_with_screen_size(&ctx_narrow, egui::vec2(240.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx_narrow, &mut app, |_| None);
    let (narrow_strip, narrow_tabs) =
      super::tab_strip::load_test_layout(&ctx_narrow).expect("missing tab strip layout metrics");
    let _ = ctx_narrow.end_frame();

    assert!(
      (wide_strip.height() - narrow_strip.height()).abs() < f32::EPSILON,
      "expected tab strip height to be constant, got wide={} narrow={}",
      wide_strip.height(),
      narrow_strip.height()
    );

    // Ensure tabs are laid out on a single row (no wrapping).
    fn distinct_rows(tab_rects: &[egui::Rect]) -> usize {
      let mut rows: Vec<f32> = Vec::new();
      for rect in tab_rects {
        let y = rect.min.y;
        if !rows.iter().any(|existing| (*existing - y).abs() < 0.5) {
          rows.push(y);
        }
      }
      rows.len()
    }

    assert_eq!(
      distinct_rows(&wide_tabs),
      1,
      "expected wide layout to have a single tab row"
    );
    assert_eq!(
      distinct_rows(&narrow_tabs),
      1,
      "expected narrow layout to have a single tab row"
    );
  }

  #[test]
  fn many_tabs_keeps_new_tab_button_clickable() {
    let mut app = BrowserAppState::new();
    for i in 0..32_u64 {
      let tab_id = TabId(i + 1);
      app.push_tab(
        BrowserTabState::new(tab_id, format!("https://example.com/{i}")),
        i == 0,
      );
    }

    let ctx = egui::Context::default();

    // Frame 1: warm up layout.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: grab the tab strip rect so we can click the "+" button (pinned to the right edge).
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let (strip_rect, _tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    // Frame 3: click the "+" button and ensure we get the expected action.
    let click_pos = egui::pos2(strip_rect.max.x - 10.0, strip_rect.center().y);
    begin_frame(&ctx, left_click_at(click_pos));
    let actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::NewTab)),
      "expected ChromeAction::NewTab, got {actions:?}"
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
