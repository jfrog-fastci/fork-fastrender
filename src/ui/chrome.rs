#![cfg(feature = "browser_ui")]

use crate::render_control::StageHeartbeat;
use crate::ui::a11y;
use crate::ui::address_bar::{format_address_bar_url, AddressBarSecurityState};
use crate::ui::browser_app::BrowserAppState;
use crate::ui::profile_autosave::BookmarkStore;
use crate::ui::load_progress::{load_progress_indicator, LoadProgressIndicator};
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::omnibox::{
  build_omnibox_suggestions_default_limit, OmniboxAction, OmniboxContext, OmniboxSearchSource,
  OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
};
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
  OpenFindInPage,
  NewTab,
  CloseTab(TabId),
  ReopenClosedTab,
  ActivateTab(TabId),
  TogglePinTab(TabId),
  NavigateTo(String),
  Back,
  Forward,
  Reload,
  StopLoading,
  AddressBarFocusChanged(bool),
  /// Toggle a bookmark for the currently active tab.
  ToggleBookmarkForActiveTab,
  /// Toggle visibility of the global history panel.
  ToggleHistoryPanel,
  /// Toggle visibility of the bookmarks manager UI.
  ToggleBookmarksManager,
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
    egui::Key::F => Key::F,
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

fn omnibox_suggestion_icon(suggestion: &OmniboxSuggestion) -> &'static str {
  match suggestion.source {
    OmniboxSuggestionSource::Primary => match &suggestion.action {
      OmniboxAction::Search(_) => "S",
      OmniboxAction::NavigateToUrl(_) => "↵",
      OmniboxAction::ActivateTab(_) => "T",
    },
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => "T",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => "ⓘ",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => "★",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => "↩",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => "H",
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => "S",
  }
}

fn omnibox_suggestion_fill_text(suggestion: &OmniboxSuggestion) -> Option<&str> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => Some(url),
    OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
    OmniboxAction::Search(query) => Some(query),
  }
}

fn omnibox_suggestion_accept_action(suggestion: &OmniboxSuggestion) -> ChromeAction {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => ChromeAction::NavigateTo(url.clone()),
    OmniboxAction::ActivateTab(tab_id) => ChromeAction::ActivateTab(*tab_id),
    OmniboxAction::Search(query) => ChromeAction::NavigateTo(query.clone()),
  }
}

pub fn chrome_ui(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
  chrome_ui_with_bookmarks(ctx, app, None, favicon_for_tab)
}

pub fn chrome_ui_with_bookmarks(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  omnibox_bookmarks: Option<&BookmarkStore>,
  mut favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
  let mut actions = Vec::new();
  let motion = UiMotion::from_env();
  let mut address_bar_rect: Option<egui::Rect> = None;
  let mut address_bar_text_edit_response: Option<egui::Response> = None;

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
    open_find_in_page,
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
    let mut open_find_in_page = false;
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
        ShortcutAction::FindInPage => open_find_in_page = true,
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
      open_find_in_page,
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
  if open_find_in_page {
    actions.push(ChromeAction::OpenFindInPage);
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

  // Clear transient tab-drag state on mouse release.
  if app.chrome.dragging_tab_id.is_some()
    && ctx.input(|i| {
      i.events.iter().any(|event| {
        matches!(
          event,
          egui::Event::PointerButton {
            pressed: false,
            button: egui::PointerButton::Primary,
            ..
          }
        )
      })
    })
  {
    app.chrome.clear_tab_drag();
  }
  egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
    actions.extend(tab_strip::tab_strip_ui(ui, app, &mut favicon_for_tab, motion));

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let is_compact = ui.available_width() < COMPACT_MODE_THRESHOLD_PX;
      let (can_back, can_forward, loading, stage, load_progress, zoom_factor) = app
        .active_tab()
        .map(|t| {
          (
            t.can_go_back,
            t.can_go_forward,
            t.loading,
            t.load_stage,
            t.load_progress,
            t.zoom,
          )
        })
        .unwrap_or((false, false, false, None, None, zoom::DEFAULT_ZOOM));

      if icon_button(ui, BrowserIcon::Back, "Back (Alt+Left)", can_back).clicked() {
        actions.push(ChromeAction::Back);
      }
      if icon_button(ui, BrowserIcon::Forward, "Forward (Alt+Right)", can_forward).clicked() {
        actions.push(ChromeAction::Forward);
      }
      if loading {
        if icon_button(ui, BrowserIcon::CloseTab, "Stop loading (Esc)", true).clicked() {
          actions.push(ChromeAction::StopLoading);
        }
      } else if icon_button(ui, BrowserIcon::Reload, "Reload (Ctrl/Cmd+R)", true).clicked() {
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
        let reset_zoom_label = format!("Zoom: {percent}% (reset)");
        let reset_zoom_response = ui
          .button(format!("{percent}%"))
          .on_hover_text("Reset zoom (Ctrl/Cmd+0)");
        reset_zoom_response.widget_info({
          let reset_zoom_label = reset_zoom_label.clone();
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, reset_zoom_label.clone())
        });
        if reset_zoom_response.clicked() {
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

      // Capture + consume navigation keys before building the text edit so they don't reach the
      // `TextEdit` (cursor movement) or bubble up to the page.
      let mut key_arrow_down = false;
      let mut key_arrow_up = false;
      let mut key_enter = false;
      let mut key_escape = false;
      if egui_focus || app.chrome.address_bar_has_focus {
        ui.input_mut(|i| {
          key_arrow_down = i.consume_key(Default::default(), egui::Key::ArrowDown);
          key_arrow_up = i.consume_key(Default::default(), egui::Key::ArrowUp);
          key_enter = i.consume_key(Default::default(), egui::Key::Enter);
          key_escape = i.consume_key(Default::default(), egui::Key::Escape);
        });
      }

      // Derive the URL for display/indicator from the active tab (not from in-progress address bar
      // edits).
      let active_url = app
        .active_tab()
        .and_then(|t| t.committed_url.as_deref().or_else(|| t.current_url()))
        .unwrap_or("")
        .to_string();
      let formatted_url = format_address_bar_url(&active_url);
      let indicator = match formatted_url.security_state {
        AddressBarSecurityState::Https => security_indicator::SecurityIndicator::Secure,
        AddressBarSecurityState::Http => security_indicator::SecurityIndicator::Insecure,
        AddressBarSecurityState::File
        | AddressBarSecurityState::About
        | AddressBarSecurityState::Other => security_indicator::SecurityIndicator::Neutral,
      };

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
      address_bar_rect = Some(bar_rect);
      if !show_text_edit {
        // When the address bar is in display mode (non-editing), still expose a focusable element
        // for assistive tech to activate.
        bar_response.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y::ADDRESS_BAR_LABEL)
        });
      }

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
            response.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, a11y::ADDRESS_BAR_LABEL)
            });
            text_edit_response = Some(response);
          } else {
            if active_url.trim().is_empty() {
              ui.add(
                egui::Label::new(
                  egui::RichText::new("Enter URL…").color(ui.visuals().weak_text_color()),
                )
                .wrap(false)
                .truncate(true),
              );
            } else {
              let font_id = egui::TextStyle::Body.resolve(ui.style());
              let mut job = egui::text::LayoutJob::default();

              if formatted_url.security_state == AddressBarSecurityState::Http {
                job.append(
                  "Not secure ",
                  0.0,
                  egui::text::TextFormat {
                    font_id: font_id.clone(),
                    color: ui.visuals().warn_fg_color,
                    ..Default::default()
                  },
                );
              }

              job.append(
                &formatted_url.display_host,
                0.0,
                egui::text::TextFormat {
                  font_id: font_id.clone(),
                  color: ui.visuals().text_color(),
                  ..Default::default()
                },
              );

              if let Some(rest) = formatted_url
                .display_path_query_fragment
                .as_deref()
                .filter(|s| !s.is_empty())
              {
                job.append(
                  &url_display::truncate_url_middle(rest, ADDRESS_BAR_DISPLAY_MAX_CHARS),
                  0.0,
                  egui::text::TextFormat {
                    font_id,
                    color: ui.visuals().weak_text_color(),
                    ..Default::default()
                  },
                );
              }

              ui.add(egui::Label::new(job).wrap(false).truncate(true));
            }
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
        // When the omnibox dropdown is open, keep keyboard focus in the address bar so keyboard
        // navigation remains stable across frames (and popups don't accidentally steal focus).
        //
        // Avoid doing this while the user is interacting with the pointer: clicks should be able to
        // change focus naturally (e.g. clicking the page to dismiss the dropdown).
        if app.chrome.omnibox.open
          && app.chrome.address_bar_has_focus
          && !ui.input(|i| i.pointer.any_pressed())
        {
          response.request_focus();
        }

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

          // User input while the omnibox is open should rebuild suggestions and reset selection.
          app.chrome.omnibox.open = true;
          app.chrome.omnibox.selected = None;
          app.chrome.omnibox.original_input = None;

          let input = app.chrome.address_bar_text.clone();
          let suggestions = {
            let ctx = OmniboxContext {
              open_tabs: &app.tabs,
              closed_tabs: &app.closed_tabs,
              visited: &app.visited,
              active_tab_id: app.active_tab_id(),
              bookmarks: omnibox_bookmarks,
              remote_search_suggest: None,
            };
            build_omnibox_suggestions_default_limit(&ctx, &input)
          };
          app.chrome.omnibox.suggestions = suggestions;
          app.chrome.omnibox.last_built_for_input = input;
          if app.chrome.omnibox.suggestions.is_empty() {
            app.chrome.omnibox.open = false;
          }
        }

        if has_focus && (key_arrow_down || key_arrow_up) {
          if !app.chrome.omnibox.open {
            app.chrome.omnibox.open = true;

            // Avoid rebuilding suggestions if we already have suggestions for the current input (for
            // example, after pressing Escape to close the dropdown while keeping focus).
            if app.chrome.omnibox.last_built_for_input != app.chrome.address_bar_text
              || app.chrome.omnibox.suggestions.is_empty()
            {
              let input = app.chrome.address_bar_text.clone();
              let suggestions = {
                let ctx = OmniboxContext {
                  open_tabs: &app.tabs,
                  closed_tabs: &app.closed_tabs,
                  visited: &app.visited,
                  active_tab_id: app.active_tab_id(),
                  bookmarks: omnibox_bookmarks,
                  remote_search_suggest: None,
                };
                build_omnibox_suggestions_default_limit(&ctx, &input)
              };
              app.chrome.omnibox.suggestions = suggestions;
              app.chrome.omnibox.last_built_for_input = input;
            }

            if app.chrome.omnibox.suggestions.is_empty() {
              app.chrome.omnibox.open = false;
            }
          }

          if app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty() {
            let len = app.chrome.omnibox.suggestions.len();
            let next = if key_arrow_down {
              match app.chrome.omnibox.selected {
                None => 0,
                Some(i) => (i + 1) % len,
              }
            } else {
              // ArrowUp
              match app.chrome.omnibox.selected {
                None => len - 1,
                Some(i) => (i + len - 1) % len,
              }
            };

            if app.chrome.omnibox.selected.is_none() && app.chrome.omnibox.original_input.is_none() {
              app.chrome.omnibox.original_input = Some(app.chrome.address_bar_text.clone());
            }
            app.chrome.omnibox.selected = Some(next);

            if let Some(suggestion) = app.chrome.omnibox.suggestions.get(next) {
              if let Some(fill) = omnibox_suggestion_fill_text(suggestion) {
                app.chrome.address_bar_text = fill.to_string();
              }
            }
          }
        }

        if has_focus && key_escape {
          if app.chrome.omnibox.open || app.chrome.omnibox.selected.is_some() {
            app.chrome.omnibox.open = false;
            app.chrome.omnibox.selected = None;
            if let Some(original) = app.chrome.omnibox.original_input.take() {
              app.chrome.address_bar_text = original;
            }
          } else {
            app.set_address_bar_editing(false);
            response.surrender_focus();
            actions.push(ChromeAction::AddressBarFocusChanged(false));
          }
        }

        if has_focus && key_enter {
          let accept_action = (app.chrome.omnibox.open)
            .then_some(())
            .and_then(|_| app.chrome.omnibox.selected)
            .and_then(|i| app.chrome.omnibox.suggestions.get(i))
            .map(omnibox_suggestion_accept_action);

          let action = accept_action.unwrap_or_else(|| {
            ChromeAction::NavigateTo(app.chrome.address_bar_text.clone())
          });

          if let ChromeAction::NavigateTo(url) = &action {
            app.chrome.address_bar_text = url.clone();
          }

          app.chrome.address_bar_editing = false;
          app.chrome.address_bar_has_focus = false;
          app.chrome.omnibox.reset();

          actions.push(action);
          actions.push(ChromeAction::AddressBarFocusChanged(false));
          response.surrender_focus();
        }

        address_bar_text_edit_response = Some(response.clone());
      }
    });
  });

  // -----------------------------------------------------------------------------
  // Omnibox dropdown overlay
  // -----------------------------------------------------------------------------
  if app.chrome.address_bar_has_focus
    && app.chrome.omnibox.open
    && !app.chrome.omnibox.suggestions.is_empty()
  {
    if let Some(anchor) = address_bar_rect {
      let pos = egui::pos2(anchor.min.x, anchor.max.y);
      let id = egui::Id::new("omnibox_dropdown");
      let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .constrain_to(ctx.screen_rect());

      let mut clicked_suggestion: Option<usize> = None;
      let inner = area.show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
          let width = anchor.width();
          if width.is_finite() && width > 0.0 {
            ui.set_min_width(width);
            ui.set_max_width(width);
          }

          const MAX_VISIBLE_ROWS: usize = 8;
          let row_height = ui.spacing().interact_size.y.max(24.0);
          let max_height = row_height * (MAX_VISIBLE_ROWS as f32);

          egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
            for (idx, suggestion) in app.chrome.omnibox.suggestions.iter().enumerate() {
              let is_selected = app.chrome.omnibox.selected == Some(idx);
              let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), row_height),
                egui::Sense::click(),
              );

              let hovered = response.hovered();
              if is_selected || hovered {
                let visuals = ui.visuals();
                let bg = if is_selected {
                  visuals.selection.bg_fill
                } else {
                  visuals.widgets.hovered.bg_fill
                };
                ui.painter().rect_filled(rect, 0.0, bg);
              }

              ui.allocate_ui_at_rect(rect, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.horizontal(|ui| {
                  ui.add_space(6.0);
                  ui.label(egui::RichText::new(omnibox_suggestion_icon(suggestion)).strong());

                  let title = suggestion
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                  let url = suggestion
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());

                  let (primary, secondary) = if let Some(title) = title {
                    (title, url)
                  } else if let Some(url) = url {
                    (url, None)
                  } else if let OmniboxAction::Search(query) = &suggestion.action {
                    (query.as_str(), None)
                  } else {
                    ("", None)
                  };

                  ui.vertical(|ui| {
                    ui.add(egui::Label::new(primary).wrap(false).truncate(true));
                    if let Some(secondary) = secondary {
                      ui.add(
                        egui::Label::new(
                          egui::RichText::new(secondary)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        )
                        .wrap(false)
                        .truncate(true),
                      );
                    }
                  });
                });
              });

              if response.clicked() {
                clicked_suggestion = Some(idx);
              }
            }
          });
        });
      });

      if let Some(idx) = clicked_suggestion {
        if let Some(suggestion) = app.chrome.omnibox.suggestions.get(idx) {
          let action = omnibox_suggestion_accept_action(suggestion);
          if let ChromeAction::NavigateTo(url) = &action {
            app.chrome.address_bar_text = url.clone();
          }
          app.chrome.address_bar_editing = false;
          app.chrome.address_bar_has_focus = false;
          app.chrome.omnibox.reset();
          actions.push(action);
          actions.push(ChromeAction::AddressBarFocusChanged(false));
          if let Some(response) = address_bar_text_edit_response.as_ref() {
            response.surrender_focus();
          }
        }
      } else {
        // Best-effort dismissal when clicking outside both the dropdown and the address bar.
        let clicked_outside = ctx.input(|i| {
          i.pointer.any_pressed()
            && i
              .pointer
              .interact_pos()
              .or_else(|| i.pointer.latest_pos())
              .is_some_and(|pos| !inner.response.rect.contains(pos) && !anchor.contains(pos))
        });
        if clicked_outside {
          app.chrome.omnibox.open = false;
          app.chrome.omnibox.selected = None;
          app.chrome.omnibox.original_input = None;
        } else if !ctx.input(|i| i.pointer.any_pressed()) {
          // Keep keyboard focus in the address bar while the dropdown is open.
          if let Some(response) = address_bar_text_edit_response.as_ref() {
            response.request_focus();
          }
        }
      }
    }
  }

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
  use super::{chrome_ui, chrome_ui_with_bookmarks, ChromeAction};
  use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
  use crate::ui::messages::TabId;
  use crate::ui::omnibox::{OmniboxSuggestionSource, OmniboxUrlSource};
  use crate::ui::profile_autosave::BookmarkStore;

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
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
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

  fn count_drawn_glyph_in_rect(output: &egui::FullOutput, glyph: &str, rect: egui::Rect) -> usize {
    fn count_in_shape(shape: &egui::epaint::Shape, glyph: &str, rect: egui::Rect) -> usize {
      match shape {
        egui::epaint::Shape::Text(text) => {
          if rect.contains(text.pos) {
            text.galley.text().matches(glyph).count()
          } else {
            0
          }
        }
        egui::epaint::Shape::Vec(shapes) => {
          shapes.iter().map(|s| count_in_shape(s, glyph, rect)).sum()
        }
        _ => 0,
      }
    }

    output
      .shapes
      .iter()
      .map(|clipped| count_in_shape(&clipped.shape, glyph, rect))
      .sum()
  }

  #[test]
  fn omnibox_suggests_bookmarked_urls() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

    let bookmarks = BookmarkStore {
      urls: ["https://example.com/bookmark".to_string()]
        .into_iter()
        .collect(),
    };

    app.chrome.request_focus_address_bar = true;
    let ctx = new_context();
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, Some(&bookmarks), |_| None);
    let _ = ctx.end_frame();

    app.chrome.address_bar_text = "example".to_string();
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, Some(&bookmarks), |_| None);
    let _ = ctx.end_frame();

    assert!(
      app.chrome.omnibox.suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://example.com/bookmark")
      }),
      "expected bookmark omnibox suggestion, got {:?}",
      app.chrome.omnibox.suggestions
    );
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com");
    assert!(!app.chrome.address_bar_has_focus);
    assert!(!app.chrome.address_bar_editing);
  }

  #[test]
  fn clicking_address_bar_display_requests_focus_and_select_all() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    begin_frame(&ctx, left_click_at(egui::pos2(400.0, 60.0)));
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.request_focus_address_bar);
    assert!(app.chrome.request_select_all_address_bar);
  }

  #[test]
  fn status_bar_shows_hovered_url() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);
    app.active_tab_mut().unwrap().hovered_url = Some("https://example.com/".to_string());

    let ctx = new_context();
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::FocusAddressBar)),
      "expected ChromeAction::FocusAddressBar, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_f_emits_open_find_in_page_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::F, modifiers);
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenFindInPage)),
      "expected ChromeAction::OpenFindInPage, got {actions:?}"
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: let egui apply the focus request.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx_wide, &mut app, None, |_| None);
    let (wide_strip, wide_tabs) =
      super::tab_strip::load_test_layout(&ctx_wide).expect("missing tab strip layout metrics");
    let _ = ctx_wide.end_frame();

    // Narrow frame.
    let ctx_narrow = egui::Context::default();
    begin_frame_with_screen_size(&ctx_narrow, egui::vec2(240.0, 600.0), Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx_narrow, &mut app, None, |_| None);
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
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: grab the tab strip rect so we can click the "+" button (pinned to the right edge).
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let (strip_rect, _tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    // Frame 3: click the "+" button and ensure we get the expected action.
    let click_pos = egui::pos2(strip_rect.max.x - 10.0, strip_rect.center().y);
    begin_frame(&ctx, left_click_at(click_pos));
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(action, ChromeAction::NewTab)),
      "expected ChromeAction::NewTab, got {actions:?}"
    );
  }

  #[test]
  fn pinned_tab_can_be_activated_by_click() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), false);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), true);
    assert!(app.pin_tab(tab_a));

    let ctx = egui::Context::default();

    // Frame 1: warm up layout.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: measure the pinned tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected pinned tab rect to be recorded");
    let _ = ctx.end_frame();

    // Frame 3: click the pinned tab.
    begin_frame(&ctx, left_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ActivateTab(id) if *id == tab_a)),
      "expected click to activate pinned tab {tab_a:?}, got {actions:?}"
    );
  }

  #[test]
  fn pinned_tabs_do_not_render_close_button() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);
    assert!(app.pin_tab(tab_a));

    let ctx = egui::Context::default();
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let (strip_rect, _tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let output = ctx.end_frame();

    // Only the unpinned tab should render a close button ("×").
    assert_eq!(count_drawn_glyph_in_rect(&output, "×", strip_rect), 1);
  }

  #[test]
  fn dragging_tab_label_reorders_tabs() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(BrowserTabState::new(tab_a, "about:newtab".to_string()), true);
    app.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);

    let ctx = egui::Context::default();

    // Frame 0: read the tab strip layout so we can target a specific tab rect (more robust than
    // hard-coded coordinates).
    begin_frame_with_screen_size(&ctx, egui::vec2(800.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    let press_pos = tab_rects.first().expect("expected first tab rect").center();
    let second = tab_rects.get(1).expect("expected second tab rect");
    let drag_pos = egui::pos2(second.center().x + 1.0, second.center().y);

    // Frame 1: press on the first tab.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![
        egui::Event::PointerMoved(press_pos),
        egui::Event::PointerButton {
          pos: press_pos,
          button: egui::PointerButton::Primary,
          pressed: true,
          modifiers: egui::Modifiers::default(),
        },
      ],
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: drag to the right (past the second tab's center).
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![egui::Event::PointerMoved(drag_pos)],
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: release.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![
        egui::Event::PointerMoved(drag_pos),
        egui::Event::PointerButton {
          pos: drag_pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: egui::Modifiers::default(),
        },
      ],
    );
    let _actions = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![tab_b, tab_a]
    );
    assert_eq!(app.active_tab_id(), Some(tab_a));
    assert!(app.chrome.dragging_tab_id.is_none());
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
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
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    let zoom_after_out = app.active_tab().unwrap().zoom;
    assert!(zoom_after_out < zoom_after_in, "expected zoom to decrease");
  }

  #[test]
  fn omnibox_typing_builds_primary_suggestion() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    // Seed history so the omnibox has local providers available in addition to the primary action.
    app
      .visited
      .record_visit("https://example.com/".to_string(), Some("Example".to_string()));

    app.chrome.address_bar_text.clear();
    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Type input that produces a primary (search) suggestion.
    begin_frame(&ctx, vec![egui::Event::Text("cats".into())]);
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !app.chrome.omnibox.suggestions.is_empty(),
      "expected omnibox suggestions to be populated"
    );
    assert!(
      app
        .chrome
        .omnibox
        .suggestions
        .iter()
        .any(|s| s.source == OmniboxSuggestionSource::Primary),
      "expected at least one OmniboxSuggestionSource::Primary, got {:?}",
      app.chrome.omnibox.suggestions
    );
  }

  #[test]
  fn omnibox_typing_about_produces_about_suggestions() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    app.chrome.address_bar_text.clear();
    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Type input that matches about pages provider.
    begin_frame(&ctx, vec![egui::Event::Text("about".into())]);
    let _ = chrome_ui(&ctx, &mut app, |_| None);
    let _ = ctx.end_frame();

    assert!(
      app
        .chrome
        .omnibox
        .suggestions
        .iter()
        .any(|s| s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::About)),
      "expected at least one OmniboxSuggestionSource::Url(About), got {:?}",
      app.chrome.omnibox.suggestions
    );
  }

  #[test]
  fn omnibox_typing_opens_and_arrow_down_previews_first_suggestion() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .visited
      .record_visit("https://example.com/".to_string(), Some("Example".to_string()));

    // Ensure typed input doesn't append to the active tab URL.
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);

    // Type input.
    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.omnibox.open);
    assert!(!app.chrome.omnibox.suggestions.is_empty());

    // ArrowDown previews first suggestion.
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com/");
  }

  #[test]
  fn omnibox_suggests_bookmarks_from_store() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.chrome.address_bar_text.clear();

    let bookmarks = BookmarkStore {
      urls: ["https://example.com/".to_string()].into_iter().collect(),
    };

    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, Some(&bookmarks), |_| None);
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);

    // Type input that matches the bookmark.
    begin_frame(&ctx, vec![egui::Event::Text("exam".into())]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, Some(&bookmarks), |_| None);
    let _ = ctx.end_frame();

    assert!(
      app.chrome.omnibox.open,
      "expected omnibox dropdown to open"
    );
    assert!(
      app.chrome.omnibox.suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://example.com/")
      }),
      "expected bookmark suggestion, got {:?}",
      app.chrome.omnibox.suggestions
    );
  }

  #[test]
  fn omnibox_escape_restores_original_input_and_keeps_focus() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .visited
      .record_visit("https://example.com/".to_string(), Some("Example".to_string()));
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com/");

    // Escape should close dropdown and restore original typed input without blurring.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(!app.chrome.omnibox.open);
    assert_eq!(app.chrome.address_bar_text, "example.com");
    assert!(app.chrome.address_bar_has_focus);
  }

  #[test]
  fn omnibox_second_escape_blurs_and_reverts_to_active_url() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .visited
      .record_visit("https://example.com/".to_string(), Some("Example".to_string()));
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    // First Escape closes dropdown and keeps focus.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Second Escape should blur and revert to active tab URL.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(!app.chrome.address_bar_has_focus);
    assert_eq!(app.chrome.address_bar_text, "about:newtab");
  }

  #[test]
  fn omnibox_enter_with_selection_emits_navigate_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .visited
      .record_visit("https://example.com/".to_string(), Some("Example".to_string()));
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NavigateTo(url) if url == "https://example.com/")),
      "expected ChromeAction::NavigateTo(\"https://example.com/\"), got {actions:?}"
    );
  }
}
