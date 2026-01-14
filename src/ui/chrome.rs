#![cfg(feature = "browser_ui")]

use crate::debug::runtime::runtime_toggles;
use crate::render_control::StageHeartbeat;
use crate::ui::a11y;
use crate::ui::address_bar::AddressBarSecurityState;
use crate::ui::appearance::{DEFAULT_UI_SCALE, MAX_UI_SCALE, MIN_UI_SCALE};
use crate::ui::bookmarks::{bookmarks_bar_ui, BookmarkStore};
use crate::ui::browser_app::{BrowserAppState, BrowserTabState, UiFocusToken};
use crate::ui::icons::paint_icon_in_rect;
use crate::ui::load_progress::{load_progress_indicator, LoadProgressIndicator};
use crate::ui::messages::TabId;
use crate::ui::motion::UiMotion;
use crate::ui::omnibox::{
  build_omnibox_suggestions_default_limit, OmniboxAction, OmniboxContext, OmniboxSearchSource,
  OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
};
use crate::ui::omnibox_nav::{
  apply_omnibox_nav_key, omnibox_suggestion_accept_action, omnibox_suggestion_fill_text,
  OmniboxNavKey,
};
use crate::ui::security_indicator;
use crate::ui::shortcuts::{map_shortcut, Key, KeyEvent, Modifiers, ShortcutAction};
use crate::ui::tab_search::{self, TabSearchMatch};
use crate::ui::theme;
use crate::ui::theme_parsing::{
  format_hex_color, parse_browser_accent_env, parse_hex_color, BrowserTheme as ThemeChoice,
  RgbaColor, ENV_BROWSER_ACCENT,
};
use crate::ui::url::{http_fallback_url_for_failed_https, resolve_omnibox_search_query};
use crate::ui::zoom;
use crate::ui::ChromeAction;
use crate::ui::{icon_button, icon_button_with_id, icon_tinted, spinner, BrowserIcon};
use std::fmt::Write as _;
use std::sync::Arc;

const ADDRESS_BAR_DISPLAY_MAX_CHARS: usize = 80;
const COMPACT_MODE_THRESHOLD_PX: f32 = 640.0;
const BOOKMARKS_BAR_MAX_ITEMS: usize = 12;
const ENV_BROWSER_SHOW_MENU_BAR: &str = "FASTR_BROWSER_SHOW_MENU_BAR";

#[derive(Debug, Clone, Default)]
struct TryHttpLabelWidthCache {
  font_id: Option<egui::FontId>,
  text_width: f32,
}

impl TryHttpLabelWidthCache {
  fn text_width(&mut self, ui: &egui::Ui, font_id: &egui::FontId) -> f32 {
    if self.font_id.as_ref() == Some(font_id) {
      return self.text_width;
    }
    self.font_id = Some(font_id.clone());
    self.text_width = ui.fonts(|f| {
      f.layout_no_wrap(
        "Try HTTP".to_string(),
        font_id.clone(),
        ui.visuals().text_color(),
      )
      .size()
      .x
    });
    self.text_width
  }
}

#[derive(Debug, Clone, Default)]
struct LoadingTextWidthCache {
  font_id: Option<egui::FontId>,
  stage: Option<StageHeartbeat>,
  width: f32,
}

impl LoadingTextWidthCache {
  fn width(
    &mut self,
    ui: &egui::Ui,
    stage: Option<StageHeartbeat>,
    font_id: &egui::FontId,
    text: &str,
  ) -> f32 {
    if self.font_id.as_ref() == Some(font_id) && self.stage == stage {
      return self.width;
    }
    self.font_id = Some(font_id.clone());
    self.stage = stage;
    self.width = ui.fonts(|f| {
      f.layout_no_wrap(text.to_owned(), font_id.clone(), ui.visuals().text_color())
        .size()
        .x
    });
    self.width
  }
}

#[derive(Debug, Clone, Default)]
struct AddressBarDisplayGalleyCache {
  // Key: the active URL generation counter from `ChromeAddressBarCache`.
  url_generation: u64,
  // Style keys: changes here should invalidate the cached galley.
  font_id: Option<egui::FontId>,
  text_color: egui::Color32,
  weak_text_color: egui::Color32,
  warn_text_color: egui::Color32,
  // Layout key: max width influences truncation.
  max_width_bits: u32,
  // Cached output.
  galley: Option<std::sync::Arc<egui::Galley>>,
}

impl AddressBarDisplayGalleyCache {
  fn update(
    &mut self,
    ui: &egui::Ui,
    formatted_url: &crate::ui::address_bar::AddressBarDisplayParts,
    display_path_query_fragment: Option<&str>,
    max_width: f32,
    url_generation: u64,
  ) {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let text_color = ui.visuals().text_color();
    let weak_text_color = ui.visuals().weak_text_color();
    let warn_text_color = ui.visuals().warn_fg_color;

    let max_width_bits = max_width.to_bits();

    let needs_rebuild = self.galley.is_none()
      || self.url_generation != url_generation
      || self.font_id.as_ref() != Some(&font_id)
      || self.text_color != text_color
      || self.weak_text_color != weak_text_color
      || self.warn_text_color != warn_text_color
      || self.max_width_bits != max_width_bits;

    if !needs_rebuild {
      return;
    }

    self.url_generation = url_generation;
    self.font_id = Some(font_id.clone());
    self.text_color = text_color;
    self.weak_text_color = weak_text_color;
    self.warn_text_color = warn_text_color;
    self.max_width_bits = max_width_bits;

    let mut job = egui::text::LayoutJob::default();

    // Mirror the previous display-mode label logic: show a security warning prefix for HTTP.
    if formatted_url.security_state == AddressBarSecurityState::Http {
      job.append(
        "Not secure ",
        0.0,
        egui::text::TextFormat {
          font_id: font_id.clone(),
          color: warn_text_color,
          ..Default::default()
        },
      );
    }

    if !formatted_url.display_host_prefix.is_empty() {
      job.append(
        &formatted_url.display_host_prefix,
        0.0,
        egui::text::TextFormat {
          font_id: font_id.clone(),
          color: weak_text_color,
          ..Default::default()
        },
      );
    }

    job.append(
      &formatted_url.display_host_domain,
      0.0,
      egui::text::TextFormat {
        font_id: font_id.clone(),
        color: text_color,
        ..Default::default()
      },
    );

    if !formatted_url.display_host_suffix.is_empty() {
      job.append(
        &formatted_url.display_host_suffix,
        0.0,
        egui::text::TextFormat {
          font_id: font_id.clone(),
          color: weak_text_color,
          ..Default::default()
        },
      );
    }

    if let Some(rest) = display_path_query_fragment {
      job.append(
        rest,
        0.0,
        egui::text::TextFormat {
          font_id,
          color: weak_text_color,
          ..Default::default()
        },
      );
    }

    // Match `Label::truncate(true)` behaviour.
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.halign = ui.layout().horizontal_placement();
    job.justify = ui.layout().horizontal_justify();

    self.galley = Some(ui.fonts(|f| f.layout_job(job)));
  }

  fn galley(&self) -> std::sync::Arc<egui::Galley> {
    self
      .galley
      .as_ref()
      .expect("AddressBarDisplayGalleyCache::update must be called before galley()")
      .clone()
  }
}

#[derive(Debug, Clone, Default)]
struct AddressBarPlaceholderGalleyCache {
  font_id: Option<egui::FontId>,
  color: egui::Color32,
  max_width_bits: u32,
  galley: Option<std::sync::Arc<egui::Galley>>,
}

impl AddressBarPlaceholderGalleyCache {
  fn update(&mut self, ui: &egui::Ui, max_width: f32) {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let max_width_bits = max_width.to_bits();

    let needs_rebuild = self.galley.is_none()
      || self.font_id.as_ref() != Some(&font_id)
      || self.color != color
      || self.max_width_bits != max_width_bits;

    if !needs_rebuild {
      return;
    }

    self.font_id = Some(font_id.clone());
    self.color = color;
    self.max_width_bits = max_width_bits;

    let mut job =
      egui::text::LayoutJob::simple("Enter URL…".to_string(), font_id, color, max_width);
    // Match `Label::truncate(true)` behaviour.
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.halign = ui.layout().horizontal_placement();
    job.justify = ui.layout().horizontal_justify();

    self.galley = Some(ui.fonts(|f| f.layout_job(job)));
  }

  fn galley(&self) -> std::sync::Arc<egui::Galley> {
    self
      .galley
      .as_ref()
      .expect("AddressBarPlaceholderGalleyCache::update must be called before galley()")
      .clone()
  }
}

#[derive(Debug, Clone, Default)]
struct FindInPageMatchLabelCache {
  active_idx: usize,
  match_count: usize,
  label: String,
}

impl FindInPageMatchLabelCache {
  fn label(&mut self, active_idx: usize, match_count: usize) -> &str {
    if self.active_idx == active_idx && self.match_count == match_count && !self.label.is_empty() {
      return self.label.as_str();
    }

    // Reserve enough space for typical `123/456` strings to avoid reallocation churn.
    if self.label.capacity() < 16 {
      self.label.reserve(16 - self.label.capacity());
    }
    self.label.clear();
    let _ = write!(&mut self.label, "{active_idx}/{match_count}");
    self.active_idx = active_idx;
    self.match_count = match_count;
    self.label.as_str()
  }
}

#[derive(Debug, Clone, Default)]
struct ZoomUiLabelCache {
  percent: u32,
  reset_label: String,
  button_label: String,
}

impl ZoomUiLabelCache {
  fn update(&mut self, percent: u32) {
    if self.percent == percent && !self.reset_label.is_empty() && !self.button_label.is_empty() {
      return;
    }

    // Reserve enough space for typical `Zoom: 123% (reset)` strings.
    if self.reset_label.capacity() < 32 {
      self.reset_label.reserve(32 - self.reset_label.capacity());
    }
    self.reset_label.clear();
    let _ = write!(&mut self.reset_label, "Zoom: {percent}% (reset)");

    if self.button_label.capacity() < 8 {
      self.button_label.reserve(8 - self.button_label.capacity());
    }
    self.button_label.clear();
    let _ = write!(&mut self.button_label, "{percent}%");

    self.percent = percent;
  }
}

#[derive(Debug, Clone, Default)]
struct DownloadsUiLabelCache {
  hover_active_count: usize,
  hover_received_bytes: u64,
  hover_total_bytes: Option<u64>,
  hover_panel_open: bool,
  hover_label: String,
  badge_active_count: usize,
  badge_label: String,
  progress_received_bytes: u64,
  progress_total_bytes: Option<u64>,
  progress_label: String,
}

impl DownloadsUiLabelCache {
  fn update(
    &mut self,
    active_count: usize,
    received_bytes: u64,
    total_bytes: Option<u64>,
    downloads_panel_open: bool,
  ) {
    if active_count > 0 {
      self.update_hover_label(active_count, received_bytes, total_bytes, downloads_panel_open);
      self.update_badge_label(active_count);
      self.update_progress_label(received_bytes, total_bytes.filter(|t| *t > 0));
    }
  }

  fn update_hover_label(
    &mut self,
    active_count: usize,
    received_bytes: u64,
    total_bytes: Option<u64>,
    downloads_panel_open: bool,
  ) {
    if self.hover_active_count == active_count
      && self.hover_received_bytes == received_bytes
      && self.hover_total_bytes == total_bytes
      && self.hover_panel_open == downloads_panel_open
      && !self.hover_label.is_empty()
    {
      return;
    }

    let toggle_label = if downloads_panel_open {
      "Hide downloads"
    } else {
      "Show downloads"
    };

    // "Show downloads: Downloading… (2) 1.2 MiB / 4.0 MiB"
    self.hover_label.clear();
    // 96 bytes is enough for typical
    // `Show downloads: Downloading… (123) 123.4 MiB / 567.8 MiB` strings.
    if self.hover_label.capacity() < 96 {
      self.hover_label.reserve(96 - self.hover_label.capacity());
    }
    self.hover_label.push_str(toggle_label);
    self.hover_label.push_str(": Downloading…");
    if active_count > 1 {
      let _ = write!(&mut self.hover_label, " ({active_count})");
    }
    self.hover_label.push(' ');
    write_bytes(&mut self.hover_label, received_bytes);
    if let Some(total) = total_bytes {
      self.hover_label.push_str(" / ");
      write_bytes(&mut self.hover_label, total);
    }

    self.hover_active_count = active_count;
    self.hover_received_bytes = received_bytes;
    self.hover_total_bytes = total_bytes;
    self.hover_panel_open = downloads_panel_open;
  }

  fn update_progress_label(&mut self, received_bytes: u64, total_bytes: Option<u64>) {
    if self.progress_received_bytes == received_bytes
      && self.progress_total_bytes == total_bytes
      && !self.progress_label.is_empty()
    {
      return;
    }

    self.progress_label.clear();
    if self.progress_label.capacity() < 64 {
      self.progress_label
        .reserve(64 - self.progress_label.capacity());
    }

    if let Some(total) = total_bytes {
      self.progress_label.push_str("Downloads progress: ");
      write_bytes(&mut self.progress_label, received_bytes);
      self.progress_label.push_str(" of ");
      write_bytes(&mut self.progress_label, total);
    } else if received_bytes > 0 {
      self.progress_label.push_str("Downloads progress: ");
      write_bytes(&mut self.progress_label, received_bytes);
    } else {
      self.progress_label.push_str("Downloads progress");
    }

    self.progress_received_bytes = received_bytes;
    self.progress_total_bytes = total_bytes;
  }

  fn update_badge_label(&mut self, active_count: usize) {
    if self.badge_active_count == active_count && !self.badge_label.is_empty() {
      return;
    }

    if self.badge_label.capacity() < 4 {
      self.badge_label.reserve(4 - self.badge_label.capacity());
    }
    self.badge_label.clear();
    if active_count > 99 {
      self.badge_label.push_str("99+");
    } else {
      let _ = write!(&mut self.badge_label, "{active_count}");
    }
    self.badge_active_count = active_count;
  }
}

fn write_bytes(out: &mut String, bytes: u64) {
  const KB: f64 = 1024.0;
  const MB: f64 = KB * 1024.0;
  const GB: f64 = MB * 1024.0;
  let b = bytes as f64;
  if b >= GB {
    let _ = write!(out, "{:.1} GiB", b / GB);
  } else if b >= MB {
    let _ = write!(out, "{:.1} MiB", b / MB);
  } else if b >= KB {
    let _ = write!(out, "{:.1} KiB", b / KB);
  } else {
    let _ = write!(out, "{bytes} B");
  }
}

mod tab_strip;

fn show_menu_bar_env_override() -> Option<bool> {
  let raw = std::env::var(ENV_BROWSER_SHOW_MENU_BAR).ok()?;
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  if raw == "1"
    || raw.eq_ignore_ascii_case("true")
    || raw.eq_ignore_ascii_case("yes")
    || raw.eq_ignore_ascii_case("on")
  {
    return Some(true);
  }
  if raw == "0"
    || raw.eq_ignore_ascii_case("false")
    || raw.eq_ignore_ascii_case("no")
    || raw.eq_ignore_ascii_case("off")
  {
    return Some(false);
  }
  None
}

fn format_bytes(bytes: u64) -> String {
  const KB: f64 = 1024.0;
  const MB: f64 = KB * 1024.0;
  const GB: f64 = MB * 1024.0;

  let b = bytes as f64;
  if b >= GB {
    format!("{:.1} GiB", b / GB)
  } else if b >= MB {
    format!("{:.1} MiB", b / MB)
  } else if b >= KB {
    format!("{:.1} KiB", b / KB)
  } else {
    format!("{bytes} B")
  }
}

const MIN_CHROME_HIT_TARGET_POINTS: f32 = 30.0;
/// Minimum pointer travel (in egui points) required to treat a primary press as a drag gesture.
///
/// This is used for chrome-level link drag/drop (drag a hovered link to the address bar) to avoid
/// triggering a navigation on normal clicks.
const LINK_DRAG_THRESHOLD_POINTS: f32 = 6.0;

#[derive(Clone, Copy)]
struct FocusRingStyle {
  stroke: egui::Stroke,
  expand: f32,
  rounding: egui::Rounding,
}

fn chrome_high_contrast_enabled(app: &BrowserAppState) -> bool {
  // Treat `FASTR_BROWSER_HIGH_CONTRAST` (when set) as an override for the profile setting so the
  // focus ring matches the currently applied theme.
  match std::env::var(crate::ui::theme_parsing::ENV_BROWSER_HIGH_CONTRAST) {
    Ok(raw) => crate::ui::theme_parsing::parse_high_contrast_env(Some(&raw))
      .ok()
      .unwrap_or(app.appearance.high_contrast),
    Err(_) => app.appearance.high_contrast,
  }
}

fn chrome_focus_ring_style(ctx: &egui::Context, app: &BrowserAppState) -> FocusRingStyle {
  let high_contrast = chrome_high_contrast_enabled(app);
  // Match the currently applied theme by taking the focus/selection stroke from egui visuals.
  let mut stroke = ctx.style().visuals.selection.stroke;
  if !stroke.width.is_finite() || stroke.width < 0.0 {
    stroke.width = 0.0;
  }
  // When the profile high-contrast toggle is enabled, ensure the ring is at least as strong as the
  // high-contrast theme variant even if the embedding hasn't applied theme changes to egui yet (e.g.
  // unit tests that render chrome in isolation).
  if high_contrast {
    stroke.width = stroke.width.max(2.0);
  }

  FocusRingStyle {
    stroke,
    // Expand beyond the widget rect so the ring reads as an outline around custom-painted widgets.
    expand: stroke.width.max(2.0),
    rounding: egui::Rounding::same(4.0),
  }
}

fn paint_focus_ring(ui: &egui::Ui, response: &egui::Response, style: FocusRingStyle) {
  if !response.has_focus() {
    return;
  }

  ui.painter().rect_stroke(
    response.rect.expand(style.expand),
    style.rounding,
    style.stroke,
  );
}

fn show_tooltip_on_hover_or_focus(ui: &egui::Ui, response: &egui::Response, tooltip: &str) {
  if !(response.hovered() || response.has_focus()) {
    return;
  }

  // `Response::on_hover_text` only triggers on pointer hover. Show the same tooltip when the widget
  // is keyboard-focused so keyboard-only users can discover icon-only controls.
  egui::show_tooltip_text(
    ui.ctx(),
    ui.make_persistent_id(("chrome_tooltip", response.id)),
    tooltip,
  );
}

fn show_tooltip_on_focus(ui: &egui::Ui, response: &egui::Response, tooltip: &str) {
  if !response.has_focus() || response.hovered() {
    return;
  }

  // `Response::on_hover_text` only triggers on pointer hover. Show the same tooltip when the widget
  // is focused via keyboard navigation.
  egui::show_tooltip_text(
    ui.ctx(),
    ui.make_persistent_id(("chrome_focus_tooltip", response.id)),
    tooltip,
  );
}

fn keyboard_activate(ui: &egui::Ui, response: &egui::Response) -> bool {
  if !response.has_focus() {
    return false;
  }

  // `egui::Button` supports Enter/Space activation when focused. Many of our chrome widgets are
  // custom-painted and wired up via `ui.interact`, so explicitly treat Enter/Space as activation.
  ui.input_mut(|i| {
    i.consume_key(Default::default(), egui::Key::Enter)
      || i.consume_key(Default::default(), egui::Key::Space)
  })
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
    egui::Key::B => Key::B,
    egui::Key::C => Key::C,
    egui::Key::D => Key::D,
    egui::Key::F => Key::F,
    egui::Key::H => Key::H,
    egui::Key::J => Key::J,
    egui::Key::K => Key::K,
    egui::Key::L => Key::L,
    egui::Key::N => Key::N,
    egui::Key::O => Key::O,
    egui::Key::P => Key::P,
    egui::Key::R => Key::R,
    egui::Key::S => Key::S,
    egui::Key::T => Key::T,
    egui::Key::V => Key::V,
    egui::Key::W => Key::W,
    egui::Key::X => Key::X,
    egui::Key::Y => Key::Y,
    egui::Key::Tab => Key::Tab,
    egui::Key::ArrowLeft => Key::Left,
    egui::Key::ArrowRight => Key::Right,
    egui::Key::Delete => Key::Delete,
    // On macOS the physical key labelled "delete" typically maps to Backspace.
    #[cfg(target_os = "macos")]
    egui::Key::Backspace => Key::Delete,
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

enum OmniboxSuggestionIcon {
  Icon(BrowserIcon),
  Text(&'static str),
}

fn omnibox_suggestion_icon(suggestion: &OmniboxSuggestion) -> OmniboxSuggestionIcon {
  match suggestion.source {
    OmniboxSuggestionSource::Primary => match &suggestion.action {
      OmniboxAction::NavigateToUrl => OmniboxSuggestionIcon::Icon(BrowserIcon::Forward),
      OmniboxAction::Search(_) => OmniboxSuggestionIcon::Icon(BrowserIcon::Search),
      OmniboxAction::ActivateTab(_) => OmniboxSuggestionIcon::Icon(BrowserIcon::Tab),
    },
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::Tab)
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::Info)
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::BookmarkFilled)
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::History)
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::History)
    }
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => {
      OmniboxSuggestionIcon::Icon(BrowserIcon::Search)
    }
  }
}

fn omnibox_suggestion_a11y_label(suggestion: &OmniboxSuggestion) -> String {
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

  match &suggestion.action {
    OmniboxAction::Search(query) => {
      let query = query.trim();
      if query.is_empty() {
        "Search".to_string()
      } else {
        format!("Search: {query}")
      }
    }
    OmniboxAction::ActivateTab(_) => {
      if let Some(title) = title {
        if let Some(url) = url {
          format!("Switch to tab: {title} ({url})")
        } else {
          format!("Switch to tab: {title}")
        }
      } else if let Some(url) = url {
        format!("Switch to tab: {url}")
      } else {
        "Switch to tab".to_string()
      }
    }
    OmniboxAction::NavigateToUrl => {
      if let Some(title) = title {
        if let Some(url) = url {
          format!("Go to: {title} ({url})")
        } else {
          format!("Go to: {title}")
        }
      } else if let Some(url) = url {
        format!("Go to: {url}")
      } else {
        "Go to URL".to_string()
      }
    }
  }
}

#[derive(Debug, Default, Clone)]
struct OmniboxSuggestionA11yLabelCache {
  labels: std::collections::HashMap<egui::Id, Arc<str>>,
}

impl OmniboxSuggestionA11yLabelCache {
  fn label(&mut self, row_id: egui::Id, suggestion: &OmniboxSuggestion) -> Arc<str> {
    if let Some(label) = self.labels.get(&row_id) {
      return Arc::clone(label);
    }

    let label: Arc<str> = Arc::from(omnibox_suggestion_a11y_label(suggestion));
    self.labels.insert(row_id, Arc::clone(&label));
    label
  }

  fn retain_row_ids(&mut self, row_ids: &[egui::Id]) {
    self.labels.retain(|id, _| row_ids.contains(id));
  }
}

fn ascii_starts_with_case_insensitive(haystack: &str, prefix: &str) -> bool {
  haystack
    .as_bytes()
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn tab_search_input_id() -> egui::Id {
  egui::Id::new("tab_search_input")
}

fn tab_search_overlay_id() -> egui::Id {
  egui::Id::new("tab_search_overlay")
}

fn tab_search_row_id(tab_id: TabId) -> egui::Id {
  egui::Id::new(("tab_search_row", tab_id))
}

fn accesskit_node_id_for_egui_id(id: egui::Id) -> accesskit::NodeId {
  // SAFETY: In egui's AccessKit integration, node ids are derived from `egui::Id`'s internal u64
  // hash value (see egui's `Id::accesskit_id`, which is crate-private). Mirror that mapping here so
  // we can reference other egui widgets via AccessKit relations like `active_descendant`.
  let raw: u64 = unsafe { std::mem::transmute::<egui::Id, u64>(id) };
  accesskit::NodeId(
    std::num::NonZeroU128::new(u128::from(raw).max(1)).expect("egui id maps to non-zero NodeId"),
  )
}

fn egui_id_from_focus_token(token: UiFocusToken) -> egui::Id {
  tab_strip::tab_strip_tab_widget_id(TabId(token.0))
}

fn restore_focus_or_clear_popup_focus(
  ctx: &egui::Context,
  restore_focus: bool,
  opener_id: Option<egui::Id>,
  popup_focus_ids: &[egui::Id],
) {
  // Avoid stealing focus when the user clicked somewhere else (or focus moved for another reason).
  let focus_in_popup = ctx.memory(|mem| popup_focus_ids.iter().any(|id| mem.has_focus(*id)));
  if !focus_in_popup {
    return;
  }

  if restore_focus {
    if let Some(opener) = opener_id {
      ctx.memory_mut(|mem| mem.request_focus(opener));
      return;
    }
  }

  // Fallback (or click-away close): explicitly surrender focus from any popup widget ids we know
  // about to avoid leaving egui focused on a widget that no longer exists ("ghost focus").
  ctx.memory_mut(|mem| {
    for id in popup_focus_ids {
      mem.surrender_focus(*id);
    }
  });
}

fn tab_search_ranked_matches(query: &str, tabs: &[BrowserTabState]) -> Vec<TabSearchMatch> {
  tab_search::ranked_matches(query, tabs)
}

fn tab_search_secondary_text(tab: &BrowserTabState) -> &str {
  let url = tab
    .committed_url
    .as_deref()
    .or_else(|| tab.current_url.as_deref())
    .unwrap_or_default();

  tab_search::http_host(url).unwrap_or(url)
}

fn tab_search_overlay_ui(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  actions: &mut Vec<ChromeAction>,
  shortcuts_enabled: bool,
  favicon_for_tab: &mut impl FnMut(TabId) -> Option<egui::TextureId>,
) {
  let overlay_id = tab_search_overlay_id();
  let was_open_id = overlay_id.with("was_open");
  let open_prev_id = overlay_id.with("open_prev");
  let opener_focus_id = overlay_id.with("opener_focus");
  let motion = UiMotion::from_ctx(ctx);
  let open_t = motion.animate_bool(
    ctx,
    overlay_id.with("popup_open"),
    app.chrome.tab_search.open,
    motion.durations.popup_open,
  );
  let open_opacity = open_t.clamp(0.0, 1.0);

  // Track the currently focused widget when tab search is opened so we can restore focus on close.
  //
  // This is stored in egui context data (rather than app state) because it is egui-specific and
  // should not leak into the egui-agnostic browser state model.
  let open_prev = ctx
    .data(|d| d.get_temp::<bool>(open_prev_id))
    .unwrap_or(false);
  if app.chrome.tab_search.open && !open_prev {
    let focused = ctx.memory(|mem| mem.focus());
    ctx.data_mut(|d| {
      d.insert_temp(opener_focus_id, focused);
    });
  }
  ctx.data_mut(|d| {
    d.insert_temp(open_prev_id, app.chrome.tab_search.open);
  });

  if shortcuts_enabled
    && app.chrome.tab_search.open
    && ctx.input(|i| i.key_pressed(egui::Key::Escape))
  {
    app.chrome.tab_search.open = false;
    actions.push(ChromeAction::CloseTabSearch);
    // Ensure the fade-out animation renders even if nothing else triggers a repaint.
    ctx.request_repaint();

    let opener = ctx
      .data(|d| d.get_temp::<Option<egui::Id>>(opener_focus_id))
      .unwrap_or(None);
    restore_focus_or_clear_popup_focus(ctx, true, opener, &[tab_search_input_id()]);
    ctx.data_mut(|d| d.insert_temp(opener_focus_id, None::<egui::Id>));
  }

  if !app.chrome.tab_search.open && open_opacity <= 0.0 {
    ctx.data_mut(|d| {
      d.insert_temp(was_open_id, false);
      d.insert_temp(open_prev_id, false);
      d.insert_temp(opener_focus_id, None::<egui::Id>);
    });
    return;
  }

  let was_open = ctx
    .data(|d| d.get_temp::<bool>(was_open_id))
    .unwrap_or(false);
  ctx.data_mut(|d| {
    d.insert_temp(was_open_id, true);
  });
  let opening = app.chrome.tab_search.open && !was_open;

  let area = egui::Area::new(overlay_id)
    .order(egui::Order::Foreground)
    .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
    .interactable(app.chrome.tab_search.open && shortcuts_enabled);

  let inner = area.show(ctx, |ui| {
    ui.set_enabled(app.chrome.tab_search.open && shortcuts_enabled);
    ui.visuals_mut().override_text_color =
      Some(with_alpha(ui.visuals().text_color(), open_opacity));
    let mut frame = egui::Frame::popup(ui.style());
    frame.fill = with_alpha(frame.fill, open_opacity);
    frame.stroke.color = with_alpha(frame.stroke.color, open_opacity);
    frame.shadow.color = with_alpha(frame.shadow.color, open_opacity);
    let frame = frame.show(ui, |ui| {
      ui.set_min_width(520.0);

      let input = ui.add(
        egui::TextEdit::singleline(&mut app.chrome.tab_search.query)
          .id(tab_search_input_id())
          .desired_width(f32::INFINITY)
          .hint_text("Search tabs…"),
      );
      input.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, a11y::TAB_SEARCH_LABEL)
      });
      let _ = ctx.accesskit_node_builder(input.id, |builder| {
        builder.set_role(accesskit::Role::SearchBox);
        builder.set_expanded(app.chrome.tab_search.open);
        builder.clear_active_descendant();
      });
      // Keep focus in the search box while the overlay is open.
      if app.chrome.tab_search.open && shortcuts_enabled {
        input.request_focus();
      }

      let query_changed = app.chrome.tab_search.open && input.changed();

      ui.separator();

      let matches_recomputed = app
        .chrome
        .tab_search
        .update_cached_matches(app.tabs_revision(), &app.tabs);
      let matches_len = app.chrome.tab_search.cached_matches().len();

      let mut selected = app.chrome.tab_search.selected;
      if query_changed {
        selected = 0;
      }

      if matches_len == 0 {
        ui.label(egui::RichText::new("No matching tabs").italics().weak());
        return None::<TabId>;
      }

      if selected >= matches_len {
        selected = matches_len - 1;
      }

      let down = app.chrome.tab_search.open && ctx.input(|i| i.key_pressed(egui::Key::ArrowDown));
      let up = app.chrome.tab_search.open && ctx.input(|i| i.key_pressed(egui::Key::ArrowUp));
      if down {
        selected = (selected + 1).min(matches_len - 1);
      } else if up {
        selected = selected.saturating_sub(1);
      }

      let enter = app.chrome.tab_search.open && ctx.input(|i| i.key_pressed(egui::Key::Enter));
      if enter {
        let tab_id = app.chrome.tab_search.cached_matches()[selected].tab_id;
        return Some(tab_id);
      }

      let mut clicked: Option<TabId> = None;
      let matches = app.chrome.tab_search.cached_matches();
      let results_list = egui::ScrollArea::vertical()
        .id_source(overlay_id.with("matches_scroll"))
        .max_height(360.0)
        .auto_shrink([false, false])
        .show_rows(
          ui,
          ui.spacing().interact_size.y.max(28.0),
          matches.len(),
          |ui, row_range| {
            let row_height = ui.spacing().interact_size.y.max(28.0);
            let rounding = egui::Rounding::same(4.0);
            let inner_margin = egui::vec2(6.0, 4.0);
            let selected_fill = ui.visuals().selection.bg_fill;
            let hovered_fill = {
              // Use a subtle text-colored scrim so hover remains visible even when the theme's
              // hovered widget fill matches the popup background.
              let base = ui.visuals().text_color();
              let alpha = if ui.visuals().dark_mode { 24 } else { 14 };
              egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha)
            };

            let scroll_selected_id = overlay_id.with("scroll_selected");
            let mut scrolled_to_selected = ctx
              .data(|d| d.get_temp::<Option<usize>>(scroll_selected_id))
              .unwrap_or(None);
            let should_scroll_selected =
              opening || down || up || query_changed || matches_recomputed;
            if should_scroll_selected {
              scrolled_to_selected = None;
            }

              // `show_rows` only constructs visible rows. To keep keyboard navigation UX identical
              // to the non-virtualized implementation, request scrolling to the selected row even
              // when it is outside `row_range`.
              if should_scroll_selected && scrolled_to_selected != Some(selected) {
                let selected_idx = selected;
                let first_row_top = ui.cursor().min.y;
                let dy_rows = selected_idx as f32 - row_range.start as f32;
                let target_top = first_row_top + dy_rows * row_height;
                let left = ui.cursor().min.x;
                let rect = egui::Rect::from_min_size(
                  egui::pos2(left, target_top),
                  egui::vec2(ui.available_width().max(0.0), row_height),
                );
                ui.scroll_to_rect(rect, Some(egui::Align::Center));
                scrolled_to_selected = Some(selected_idx);
              }

              for idx in row_range {
                let m = matches[idx];
                let tab = &mut app.tabs[m.tab_index];
                let is_selected = idx == selected;

                let title = tab.display_title();
                let secondary = tab_search_secondary_text(tab);
                let a11y_label = tab.tab_search_row_accessible_label(title, secondary);

                // Use an explicit per-tab widget id so the AccessKit node id remains stable even if
                // the filtered matches list reorders while the overlay is open.
                let row_id = tab_search_row_id(tab.id);
                let (_, rect) =
                  ui.allocate_space(egui::vec2(ui.available_width().max(0.0), row_height));
                let response = ui.interact(rect, row_id, egui::Sense::click());
                response.widget_info({
                  let label = a11y_label.clone();
                  let selected = is_selected;
                  move || {
                    egui::WidgetInfo::selected(egui::WidgetType::Button, selected, label.clone())
                  }
                });
                let _ = ctx.accesskit_node_builder(response.id, |builder| {
                  builder.set_role(accesskit::Role::ListBoxOption);
                  builder.set_selected(is_selected);
                });

                let hover_t = motion.animate_bool(
                  ui.ctx(),
                  row_id.with("hover"),
                  response.hovered(),
                  motion.durations.hover_fade,
                );
                let selected_t = motion.animate_bool(
                  ui.ctx(),
                  row_id.with("selected"),
                  is_selected,
                  motion.durations.hover_fade,
                );
                if hover_t > 0.0 {
                  ui.painter().rect_filled(
                    rect,
                    rounding,
                    with_alpha(hovered_fill, hover_t * open_opacity),
                  );
                }
                if selected_t > 0.0 {
                  ui.painter().rect_filled(
                    rect,
                    rounding,
                    with_alpha(selected_fill, selected_t * open_opacity),
                  );
                }

                ui.allocate_ui_at_rect(rect.shrink2(inner_margin), |ui| {
                  ui.horizontal(|ui| {
                    let mut drew_favicon = false;
                    if let Some(tex_id) = favicon_for_tab(tab.id) {
                      let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                      if let Some(meta) = tab.favicon_meta {
                        let (w, h) = meta.size_px;
                        if w > 0 && h > 0 {
                          let height_points = 16.0;
                          let aspect = (w as f32) / (h as f32);
                          let width_points = (height_points * aspect).clamp(8.0, 32.0);
                          let (_id, rect) =
                            ui.allocate_space(egui::vec2(width_points, height_points));
                          if ui.is_rect_visible(rect) {
                            ui.painter().image(tex_id, rect, uv, egui::Color32::WHITE);
                          }
                          drew_favicon = true;
                        }
                      }
                      if !drew_favicon {
                        let (_id, rect) = ui.allocate_space(egui::vec2(16.0, 16.0));
                        if ui.is_rect_visible(rect) {
                          ui.painter().image(tex_id, rect, uv, egui::Color32::WHITE);
                        }
                        drew_favicon = true;
                      }
                    }
                    if !drew_favicon {
                      ui.add_space(16.0);
                    }

                    ui.vertical(|ui| {
                      ui.label(egui::RichText::new(title).strong());
                      ui.label(egui::RichText::new(secondary).small().weak());
                    });
                  });
                });

                if app.chrome.tab_search.open && response.hovered() && !(down || up) {
                  selected = idx;
                }
                if app.chrome.tab_search.open && response.clicked() {
                  clicked = Some(tab.id);
                }
              }

              ctx.data_mut(|d| {
                d.insert_temp(scroll_selected_id, scrolled_to_selected);
              });
          },
        );
      let _ = ctx.accesskit_node_builder(results_list.id, |builder| {
        builder.set_role(accesskit::Role::ListBox);
        builder.set_name("Tab search results".to_string());
      });

      if app.chrome.tab_search.open {
        if let Some(m) = matches.get(selected) {
          let active_descendant = accesskit_node_id_for_egui_id(tab_search_row_id(m.tab_id));
          let _ = ctx.accesskit_node_builder(input.id, |builder| {
            builder.set_role(accesskit::Role::SearchBox);
            builder.set_expanded(app.chrome.tab_search.open);
            builder.set_active_descendant(active_descendant);
          });
        }
      }

      app.chrome.tab_search.selected = selected;
      clicked
    });

    frame.inner
  });
  let action = inner.inner;

  // Click-away dismissal (common quick-switcher UX).
  //
  // Note that we do not attempt to "consume" the click: closing the overlay on a tab click should
  // still activate that tab, matching typical menu dismissal behaviour.
  if app.chrome.tab_search.open {
    let overlay_rect = inner.response.rect;
    let clicked_outside = ctx.input(|i| {
      i.events.iter().any(|event| match event {
        egui::Event::PointerButton {
          pos, pressed: true, ..
        } => !overlay_rect.contains(*pos),
        _ => false,
      })
    });
    if clicked_outside {
      app.chrome.tab_search.open = false;
      actions.push(ChromeAction::CloseTabSearch);
      ctx.request_repaint();

      // Pointer-driven dismissal: do not jump focus back to the opener, but ensure focus isn't left
      // on the now-hidden input.
      restore_focus_or_clear_popup_focus(ctx, false, None, &[tab_search_input_id()]);
      ctx.data_mut(|d| d.insert_temp(opener_focus_id, None::<egui::Id>));
      return;
    }

    if let Some(tab_id) = action {
      let restore = ctx.input(|i| i.key_pressed(egui::Key::Enter));
      app.chrome.tab_search.open = false;
      actions.push(ChromeAction::ActivateTab(tab_id));
      actions.push(ChromeAction::CloseTabSearch);
      ctx.request_repaint();

      let opener = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(opener_focus_id))
        .unwrap_or(None);
      restore_focus_or_clear_popup_focus(ctx, restore, opener, &[tab_search_input_id()]);
      ctx.data_mut(|d| d.insert_temp(opener_focus_id, None::<egui::Id>));
      return;
    }
  }
}

pub fn chrome_ui(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  chrome_has_text_focus: bool,
  shortcuts_enabled: bool,
  favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
  chrome_ui_with_bookmarks(
    ctx,
    app,
    None,
    chrome_has_text_focus,
    shortcuts_enabled,
    favicon_for_tab,
  )
}

pub fn chrome_ui_with_bookmarks(
  ctx: &egui::Context,
  app: &mut BrowserAppState,
  omnibox_bookmarks: Option<&BookmarkStore>,
  chrome_has_text_focus: bool,
  shortcuts_enabled: bool,
  mut favicon_for_tab: impl FnMut(TabId) -> Option<egui::TextureId>,
) -> Vec<ChromeAction> {
  theme::apply_high_contrast_if_enabled(ctx);
  let focus_ring = chrome_focus_ring_style(ctx, app);

  let mut actions = Vec::new();
  UiMotion::set_ctx_reduced_motion(ctx, app.appearance.reduced_motion);
  let motion = UiMotion::from_ctx(ctx);
  let mut address_bar_rect: Option<egui::Rect> = None;
  let mut address_bar_text_edit_response: Option<egui::Response> = None;

  // Tab context menu state (right-click on a tab).
  //
  // This is browser-chrome UI state, so keep it local to the chrome layer (rather than the worker).
  if let Some(open_menu) = app.chrome.open_tab_context_menu {
    let close_on_blur = ctx.input(|i| !i.focused);
    let close_on_escape = shortcuts_enabled && ctx.input(|i| i.key_pressed(egui::Key::Escape));
    if close_on_blur || close_on_escape {
      let menu_id = egui::Id::new(("tab_context_menu", open_menu.tab_id));
      let focus_ids = ctx
        .data(|d| d.get_temp::<Vec<egui::Id>>(menu_id.with("popup_focus_ids")))
        .unwrap_or_default();
      restore_focus_or_clear_popup_focus(
        ctx,
        close_on_escape,
        open_menu.opener_focus.map(egui_id_from_focus_token),
        &focus_ids,
      );
      app.chrome.open_tab_context_menu = None;
      app.chrome.tab_context_menu_rect = None;
    }
  }

  // Dismiss the menu on click outside.
  if app.chrome.open_tab_context_menu.is_some() {
    if let Some((min_x, min_y, max_x, max_y)) = app.chrome.tab_context_menu_rect {
      let clicked_outside = ctx.input(|i| {
        i.events.iter().any(|event| match event {
          egui::Event::PointerButton {
            pos, pressed: true, ..
          } => pos.x < min_x || pos.x > max_x || pos.y < min_y || pos.y > max_y,
          _ => false,
        })
      });
      if clicked_outside {
        if let Some(open_menu) = app.chrome.open_tab_context_menu {
          let menu_id = egui::Id::new(("tab_context_menu", open_menu.tab_id));
          let focus_ids = ctx
            .data(|d| d.get_temp::<Vec<egui::Id>>(menu_id.with("popup_focus_ids")))
            .unwrap_or_default();
          restore_focus_or_clear_popup_focus(ctx, false, None, &focus_ids);
        }
        app.chrome.open_tab_context_menu = None;
        app.chrome.tab_context_menu_rect = None;
      }
    }
  } else {
    app.chrome.tab_context_menu_rect = None;
  }

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
  // Ctrl/Cmd+L is expected to focus the address bar regardless of which chrome widget currently
  // has focus (e.g. find-in-page input, history search). Treat it as a global browser shortcut.
  let allow_focus_address_bar = true;
  let allow_history_navigation = if cfg!(target_os = "macos") {
    // On macOS, the canonical back/forward shortcuts are Cmd+[ / Cmd+]. These do not interfere with
    // word-wise cursor movement in text fields, so allow history navigation even while the address
    // bar is focused.
    !chrome_has_text_focus || app.chrome.address_bar_has_focus
  } else {
    // On other platforms, back/forward is typically Alt+Left/Right, which is also used for
    // word-wise cursor movement in some text fields. Suppress history navigation while the address
    // bar is focused to avoid stealing those editing gestures.
    !chrome_has_text_focus && !app.chrome.address_bar_has_focus
  };
  let (
    focus_address_bar,
    new_window,
    open_find_in_page,
    save_page,
    print_page,
    toggle_bookmarks_manager,
    toggle_downloads_panel,
    new_tab,
    close_tab,
    reopen_closed_tab,
    open_tab_search,
    reload,
    home,
    toggle_bookmark,
    toggle_history_panel,
    toggle_bookmarks_bar,
    open_clear_browsing_data_dialog,
    tab_delta,
    tab_number,
    back,
    forward,
    zoom_action,
  ) = if shortcuts_enabled {
    ctx.input(|i| {
      // Use the key event's modifier snapshot rather than `i.modifiers`: the winit integration feeds
      // modifiers via events, and using the event snapshot keeps this robust in unit tests as well.
      let mut focus_address_bar = false;
      let mut new_window = false;
      let mut open_find_in_page = false;
      let mut save_page = false;
      let mut print_page = false;
      let mut toggle_bookmarks_manager = false;
      let mut toggle_downloads_panel = false;
      let mut new_tab = false;
      let mut close_tab = false;
      let mut reopen_closed_tab = false;
      let mut open_tab_search = false;
      let mut reload = false;
      let mut home = false;
      let mut toggle_bookmark = false;
      let mut toggle_history_panel = false;
      let mut toggle_bookmarks_bar = false;
      let mut open_clear_browsing_data_dialog = false;
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
          ShortcutAction::NewWindow => new_window = true,
          ShortcutAction::FindInPage => open_find_in_page = true,
          ShortcutAction::SavePage => save_page = true,
          ShortcutAction::PrintPage => print_page = true,
          ShortcutAction::ToggleBookmarksManager => toggle_bookmarks_manager = true,
          ShortcutAction::ToggleDownloadsPanel => toggle_downloads_panel = true,
          ShortcutAction::NewTab => new_tab = true,
          ShortcutAction::CloseTab => close_tab = true,
          ShortcutAction::ReopenClosedTab => reopen_closed_tab = true,
          ShortcutAction::OpenTabSearch => open_tab_search = true,
          ShortcutAction::Reload => reload = true,
          ShortcutAction::GoHome => home = true,
          ShortcutAction::ToggleBookmark => toggle_bookmark = true,
          ShortcutAction::ShowHistory => toggle_history_panel = true,
          ShortcutAction::ShowBookmarksManager => toggle_bookmarks_manager = true,
          ShortcutAction::ToggleBookmarksBar => toggle_bookmarks_bar = true,
          ShortcutAction::OpenClearBrowsingDataDialog => open_clear_browsing_data_dialog = true,
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
        new_window,
        open_find_in_page,
        save_page,
        print_page,
        toggle_bookmarks_manager,
        toggle_downloads_panel,
        new_tab,
        close_tab,
        reopen_closed_tab,
        open_tab_search,
        reload,
        home,
        toggle_bookmark,
        toggle_history_panel,
        toggle_bookmarks_bar,
        open_clear_browsing_data_dialog,
        tab_delta,
        tab_number,
        back,
        forward,
        zoom_action,
      )
    })
  } else {
    (
      false, false, false, false, false, false, false, false, false, false, false, false, false,
      false, false, false, false, None, None, false, false, None,
    )
  };

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
  if shortcuts_enabled {
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
  }

  if focus_address_bar {
    actions.push(ChromeAction::FocusAddressBar);
    // Apply the focus/select changes immediately (this frame) so the address bar widget can
    // consume them when it's built below.
    app.chrome.request_focus_address_bar = true;
    app.chrome.request_select_all_address_bar = true;
  }
  if new_window {
    actions.push(ChromeAction::NewWindow);
  }
  if open_find_in_page {
    actions.push(ChromeAction::OpenFindInPage);
    // Ctrl/Cmd+F should close the omnibox dropdown so it doesn't keep focus in the address bar.
    app.chrome.omnibox.reset();
  }
  if save_page {
    actions.push(ChromeAction::SavePage);
  }
  if print_page {
    actions.push(ChromeAction::PrintPage);
  }
  if toggle_bookmarks_manager {
    actions.push(ChromeAction::ToggleBookmarksManager);
  }
  if toggle_downloads_panel {
    actions.push(ChromeAction::ToggleDownloadsPanel);
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
  if open_tab_search && !app.chrome.tab_search.open {
    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query.clear();
    app.chrome.tab_search.selected = 0;
    // Opening tab search is a modal interaction; cancel address bar editing/focus so typed input
    // doesn't stay "stuck" in the address bar behind the overlay.
    app.set_address_bar_editing(false);
    app.chrome.request_focus_address_bar = false;
    app.chrome.request_select_all_address_bar = false;
    app.chrome.omnibox.reset();
    actions.push(ChromeAction::OpenTabSearch);
  }
  if reload {
    actions.push(ChromeAction::Reload);
  }
  if home {
    actions.push(ChromeAction::Home);
  }
  if toggle_bookmark {
    actions.push(ChromeAction::ToggleBookmarkForActiveTab);
  }
  if toggle_history_panel {
    actions.push(ChromeAction::ToggleHistoryPanel);
  }
  if toggle_bookmarks_bar {
    actions.push(ChromeAction::ToggleBookmarksBar);
  }
  if open_clear_browsing_data_dialog {
    actions.push(ChromeAction::OpenClearBrowsingDataDialog);
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
          app.chrome.open_tab_context_menu = None;
          app.chrome.tab_context_menu_rect = None;
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
        app.chrome.open_tab_context_menu = None;
        app.chrome.tab_context_menu_rect = None;
      }
    }
  }

  if back {
    actions.push(ChromeAction::Back);
  }
  if forward {
    actions.push(ChromeAction::Forward);
  }

  tab_search_overlay_ui(
    ctx,
    app,
    &mut actions,
    shortcuts_enabled,
    &mut favicon_for_tab,
  );
  let mut appearance_button_rect: Option<egui::Rect> = None;
  let mut appearance_opened_now = false;
  egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
    // Ensure icon-only chrome buttons meet the minimum hit target size.
    let interact_size = ui.spacing().interact_size;
    ui.spacing_mut().interact_size = egui::vec2(
      interact_size.x.max(MIN_CHROME_HIT_TARGET_POINTS),
      interact_size.y.max(MIN_CHROME_HIT_TARGET_POINTS),
    );

    actions.extend(tab_strip::tab_strip_ui(
      ui,
      app,
      &mut favicon_for_tab,
      motion,
      focus_ring,
    ));

    ui.separator();

    // Navigation + address bar row.
    ui.horizontal(|ui| {
      let is_compact = ui.available_width() < COMPACT_MODE_THRESHOLD_PX;
      let active_tab = app.active_tab();
      let (can_back, can_forward, loading, stage, load_progress, zoom_factor) = active_tab
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
      // Avoid cloning error/warning strings every frame; these are only needed by reference for
      // display/tooltip rendering.
      let error = active_tab.and_then(|t| t.error.as_deref());
      let warning = active_tab.and_then(|t| t.warning.as_deref());

      let downloads = app.downloads.aggregate_progress();

      let back_tooltip = if cfg!(target_os = "macos") {
        "Back (Cmd+[)"
      } else {
        "Back (Alt+Left)"
      };
      let back_response = icon_button(ui, BrowserIcon::Back, back_tooltip, can_back);
      #[cfg(test)]
      store_test_id(ctx, "chrome_back_button_id", back_response.id);
      if back_response.clicked() {
        actions.push(ChromeAction::Back);
      }

      let forward_tooltip = if cfg!(target_os = "macos") {
        "Forward (Cmd+])"
      } else {
        "Forward (Alt+Right)"
      };
      let forward_response = icon_button(ui, BrowserIcon::Forward, forward_tooltip, can_forward);
      #[cfg(test)]
      store_test_id(ctx, "chrome_forward_button_id", forward_response.id);
      if forward_response.clicked() {
        actions.push(ChromeAction::Forward);
      }
      if loading {
        let response = icon_button(ui, BrowserIcon::StopLoading, "Stop loading (Esc)", true);
        #[cfg(test)]
        store_test_id(ctx, "chrome_reload_stop_button_id", response.id);
        if response.clicked() {
          actions.push(ChromeAction::StopLoading);
        }
      } else {
        let response = icon_button(ui, BrowserIcon::Reload, "Reload (Ctrl/Cmd+R)", true);
        #[cfg(test)]
        store_test_id(ctx, "chrome_reload_stop_button_id", response.id);
        if response.clicked() {
          actions.push(ChromeAction::Reload);
        }
      }
      let home_tooltip = if cfg!(target_os = "macos") {
        "Home (Cmd+Shift+H)"
      } else {
        "Home (Alt+Home)"
      };
      let home_response = icon_button(ui, BrowserIcon::Home, home_tooltip, true);
      #[cfg(test)]
      store_test_id(ctx, "chrome_home_button_id", home_response.id);
      if home_response.clicked() {
        actions.push(ChromeAction::Home);
      }

      // Zoom controls (optional, but useful for discoverability and as a fallback on platforms with
      // non-US keyboard layouts).
      //
      // In compact mode, keep the chrome minimal, but still surface a zoom indicator when the zoom
      // is non-default so users can discover and reset it.
      let zoom_non_default = (zoom_factor - zoom::DEFAULT_ZOOM).abs() > 1e-3;
      if is_compact {
        if zoom_non_default {
          let percent = zoom::zoom_percent(zoom_factor);
          let zoom_label_cache_id = ui.make_persistent_id("zoom_label_cache");
          let mut zoom_label_cache: ZoomUiLabelCache = ctx.data_mut(|d| {
            std::mem::take(d.get_temp_mut_or_default::<ZoomUiLabelCache>(zoom_label_cache_id))
          });
          zoom_label_cache.update(percent);
          let reset_zoom_label = zoom_label_cache.reset_label.as_str();
          let reset_btn_label = zoom_label_cache.button_label.as_str();

          let reset_btn = egui::Button::new(reset_btn_label).min_size(egui::vec2(
            MIN_CHROME_HIT_TARGET_POINTS,
            MIN_CHROME_HIT_TARGET_POINTS,
          ));
          let reset_zoom_response = ui.add(reset_btn);
          #[cfg(test)]
          store_test_id(ctx, "chrome_zoom_reset_button_id", reset_zoom_response.id);
          show_tooltip_on_hover_or_focus(ui, &reset_zoom_response, "Reset zoom (Ctrl/Cmd+0)");
          paint_focus_ring(ui, &reset_zoom_response, focus_ring);
          reset_zoom_response.widget_info({
            let label = reset_zoom_label;
            move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
          });
          if reset_zoom_response.clicked() {
            if let Some(tab) = app.active_tab_mut() {
              tab.zoom = zoom::zoom_reset();
            }
          }

          ctx.data_mut(|d| d.insert_temp(zoom_label_cache_id, zoom_label_cache));
        }
      } else {
        let response = icon_button(ui, BrowserIcon::ZoomOut, "Zoom out (Ctrl/Cmd+-)", true);
        #[cfg(test)]
        store_test_id(ctx, "chrome_zoom_out_button_id", response.id);
        if response.clicked() {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_out(tab.zoom);
          }
        }
        let percent = zoom::zoom_percent(zoom_factor);
        let zoom_label_cache_id = ui.make_persistent_id("zoom_label_cache");
        let mut zoom_label_cache = if zoom_non_default {
          Some(ctx.data_mut(|d| {
            std::mem::take(d.get_temp_mut_or_default::<ZoomUiLabelCache>(zoom_label_cache_id))
          }))
        } else {
          None
        };
        let (reset_zoom_label, reset_btn_label) = if let Some(cache) = zoom_label_cache.as_mut() {
          cache.update(percent);
          (cache.reset_label.as_str(), cache.button_label.as_str())
        } else {
          ("Zoom: 100% (reset)", "100%")
        };
        let reset_btn = egui::Button::new(reset_btn_label).min_size(egui::vec2(
          MIN_CHROME_HIT_TARGET_POINTS,
          MIN_CHROME_HIT_TARGET_POINTS,
        ));
        let reset_zoom_response = ui.add(reset_btn);
        #[cfg(test)]
        store_test_id(ctx, "chrome_zoom_reset_button_id", reset_zoom_response.id);
        show_tooltip_on_hover_or_focus(ui, &reset_zoom_response, "Reset zoom (Ctrl/Cmd+0)");
        paint_focus_ring(ui, &reset_zoom_response, focus_ring);
        reset_zoom_response.widget_info({
          let label = reset_zoom_label;
          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
        });
        if reset_zoom_response.clicked() {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_reset();
          }
        }
        if let Some(cache) = zoom_label_cache {
          ctx.data_mut(|d| d.insert_temp(zoom_label_cache_id, cache));
        }
        let response = icon_button(ui, BrowserIcon::ZoomIn, "Zoom in (Ctrl/Cmd++)", true);
        #[cfg(test)]
        store_test_id(ctx, "chrome_zoom_in_button_id", response.id);
        if response.clicked() {
          if let Some(tab) = app.active_tab_mut() {
            tab.zoom = zoom::zoom_in(tab.zoom);
          }
        }
      }

      // ---------------------------------------------------------------------------
      // Address bar (pill + truncation + security indicator)
      // ---------------------------------------------------------------------------
      //
      // Keep widget insertion order left-to-right so Tab/Shift+Tab traversal matches the
      // visual order. `Layout::right_to_left` is convenient for placement, but egui's focus
      // navigation follows insertion order, not visual layout.
      let address_bar_id = ui.make_persistent_id("address_bar");
      let address_bar_display_id = address_bar_id.with("display");
      #[cfg(test)]
      store_test_id(ctx, "chrome_address_bar_text_edit_id", address_bar_id);
      #[cfg(test)]
      store_test_id(ctx, "chrome_address_bar_display_id", address_bar_display_id);

      let egui_focus = ctx.memory(|mem| mem.has_focus(address_bar_id));
      let display_focus = ctx.memory(|mem| mem.has_focus(address_bar_display_id));
      let show_text_edit_initial = egui_focus
        || display_focus
        || app.chrome.address_bar_has_focus
        || app.chrome.request_focus_address_bar;

      // If the address bar's display pill is focused (e.g. via Tab traversal), promote it to the
      // real `TextEdit` so typing works as expected.
      if display_focus {
        app.chrome.request_focus_address_bar = true;
        app.chrome.request_select_all_address_bar = true;
      }

      // Capture + consume navigation keys (ArrowUp/Down/Enter/Escape) when the address bar is in
      // text-edit mode so they don't reach the `TextEdit` (cursor movement) or bubble up to the
      // page.
      //
      // NOTE: We intentionally *don't* consume keys based solely on the initial focus state:
      // winit/egui can batch a focus change (click or Tab) and the first keystroke into the same
      // frame, so we need to wait until after `activated_display_mode` is computed below.
      let mut key_arrow_down = false;
      let mut key_arrow_up = false;
      let mut key_tab = false;
      let mut key_alt_enter = false;
      let mut key_enter = false;
      let mut key_escape = false;

      // Derive the URL for display/indicator from the active tab (not from in-progress address bar
      // edits).
      let active_url = app
        .active_tab()
        .and_then(|t| t.committed_url.as_deref().or_else(|| t.current_url()))
        .unwrap_or("");
      let active_url_trim = active_url.trim();

      let (
        formatted_url,
        url_generation,
        indicator,
        display_path_query_fragment,
        active_url_is_bookmarked,
        loading_text,
      ) = {
        let cache = &mut app.chrome.address_bar_cache;
        cache.update_active_url(active_url, ADDRESS_BAR_DISPLAY_MAX_CHARS);
        let url_generation = cache.active_url_generation();
        let active_url_is_bookmarked = cache.is_url_bookmarked(active_url_trim, omnibox_bookmarks);
        let indicator = cache.security_indicator();
        let loading_text = cache.loading_text(stage);
        let formatted_url = cache.formatted_url();
        let display_path_query_fragment = cache.display_path_query_fragment();
        (
          formatted_url,
          url_generation,
          indicator,
          display_path_query_fragment,
          active_url_is_bookmarked,
          loading_text,
        )
      };
      let bar_height = ui.spacing().interact_size.y;
      let button_side = ui.spacing().interact_size.y;
      let spacing_x = ui.spacing().item_spacing.x;
      // Reserve space for the right-side menu + appearance buttons (+ spacing between them).
      let reserved_right = button_side * 2.0 + spacing_x * 2.0;
      let (_id, bar_rect) = ui.allocate_space(egui::vec2(
        (ui.available_width() - reserved_right).max(0.0),
        bar_height,
      ));
      let mut bar_response = ui.interact(
        bar_rect,
        address_bar_display_id,
        if show_text_edit_initial {
          egui::Sense::hover()
        } else {
          egui::Sense::click()
        },
      );

      // Secondary-click on the address bar in display mode should open a context menu (Copy URL,
      // etc.) without promoting the pill into edit mode.
      //
      // In egui, a pointer click can also assign keyboard focus. If the display pill receives focus
      // it will be promoted into the `TextEdit` on the following frame (see `display_focus` above),
      // which is not desired for a context-menu gesture. Explicitly surrender focus on secondary
      // clicks to preserve the current UX (right click opens menu, primary click enters editing).
      let context_menu_click = !show_text_edit_initial && bar_response.secondary_clicked();
      if context_menu_click {
        bar_response.surrender_focus();
      }

      // When the address bar gains focus in display mode (e.g. via Tab), immediately open the real
      // text field so subsequent typing works (and so we don't drop a batched Tab+Text input frame).
      let activated_by_keyboard = keyboard_activate(ui, &bar_response);
      let activated_display_mode = !show_text_edit_initial
        && !context_menu_click
        && (bar_response.clicked() || bar_response.has_focus() || activated_by_keyboard);
      if activated_display_mode {
        app.chrome.request_focus_address_bar = true;
        app.chrome.request_select_all_address_bar = true;
        ctx.request_repaint();
      }
      let show_text_edit = show_text_edit_initial || activated_display_mode;

      if show_text_edit && shortcuts_enabled {
        // If we have an inline-completion suffix selected, treat Tab as "accept completion" instead
        // of focus traversal.
        //
        // We consume Tab here (before the `TextEdit` sees it) so egui doesn't move focus to the next
        // widget.
        let tab_should_accept_inline_completion = egui_focus
          && app.chrome.omnibox.selected.is_none()
          && app.chrome.omnibox.original_input.is_some()
          && {
            let typed = app
              .chrome
              .omnibox
              .original_input
              .as_deref()
              .unwrap_or_default();
            let typed_len = typed.chars().count();
            let completed_len = app.chrome.address_bar_text.chars().count();
            if typed_len >= completed_len {
              false
            } else {
              let state =
                egui::text_edit::TextEditState::load(ctx, address_bar_id).unwrap_or_default();
              state.ccursor_range().is_some_and(|range| {
                range.primary.index == typed_len && range.secondary.index == completed_len
              })
            }
          };
        ui.input_mut(|i| {
          key_arrow_down = i.consume_key(Default::default(), egui::Key::ArrowDown);
          key_arrow_up = i.consume_key(Default::default(), egui::Key::ArrowUp);
          if tab_should_accept_inline_completion {
            key_tab = i.consume_key(Default::default(), egui::Key::Tab);
          }
          // Consume Alt+Enter separately from plain Enter so we can implement the standard
          // "open in new tab" omnibox behaviour (Alt+Enter on Windows/Linux, Option+Enter on
          // macOS).
          key_alt_enter = i.consume_key(
            egui::Modifiers {
              alt: true,
              ..Default::default()
            },
            egui::Key::Enter,
          );
          key_enter = i.consume_key(Default::default(), egui::Key::Enter);
          key_escape = i.consume_key(Default::default(), egui::Key::Escape);
        });
      }
      address_bar_rect = Some(bar_rect);
      #[cfg(test)]
      store_test_rect(ctx, "chrome_address_bar_rect", bar_rect);
      if !show_text_edit {
        // When the address bar is in display mode (non-editing), still expose a focusable element
        // for assistive tech to activate.
        bar_response.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y::ADDRESS_BAR_LABEL)
        });
      }

      let bar_rounding = egui::Rounding::same(bar_rect.height() / 2.0);
      ui.painter().rect_filled(
        bar_rect,
        bar_rounding,
        ui.visuals().widgets.inactive.bg_fill,
      );

      // Build the contents inside an inset rect to get pill-like padding.
      let pad = ui.spacing().button_padding;
      let inner_rect = bar_rect.shrink2(egui::vec2(pad.x.max(6.0), pad.y.max(4.0)));
      let mut text_edit_response: Option<egui::Response> = None;
      ui.allocate_ui_at_rect(inner_rect, |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        // Split into left (security icon + URL) and right (status indicators + actions) regions so
        // Tab focus traversal follows the visual left-to-right order while preserving truncation.
        let badge_rounding =
          egui::Rounding::same((ui.visuals().widgets.inactive.rounding.nw * 0.4).clamp(2.0, 4.0));
        let badge_margin = {
          let pad = ui.spacing().button_padding;
          egui::Margin::same((pad.y * 0.35).clamp(1.0, 3.0))
        };

        let err_msg = error.as_deref().filter(|s| !s.trim().is_empty());
        let show_try_http = err_msg.is_some()
          && matches!(formatted_url.security_state, AddressBarSecurityState::Https);
        let err_t = motion.animate_bool(
          ctx,
          address_bar_id.with("status_badge_error"),
          err_msg.is_some(),
          motion.durations.progress_fade,
        );

        let warn_msg = warning.as_deref().filter(|s| !s.trim().is_empty());
        let warn_t = motion.animate_bool(
          ctx,
          address_bar_id.with("status_badge_warning"),
          warn_msg.is_some(),
          motion.durations.progress_fade,
        );

        let full_rect = ui.max_rect();
        let item_spacing = ui.spacing().item_spacing.x;
        let button_side = ui.spacing().interact_size.y;
        let icon_side = ui.spacing().icon_width;
        let badge_side = icon_side + badge_margin.left + badge_margin.right;
        let try_http_button_width = if show_try_http {
          let font_id = egui::TextStyle::Small.resolve(ui.style());
          let text_width = ctx.data_mut(|d| {
            let cache_id = address_bar_id.with("try_http_label_width_cache");
            let cache = d.get_temp_mut_or_default::<TryHttpLabelWidthCache>(cache_id);
            cache.text_width(ui, &font_id)
          });
          // Account for egui's button padding.
          text_width + ui.spacing().button_padding.x * 2.0
        } else {
          0.0
        };
        // Avoid per-frame heap allocation here: this runs every frame even while idle.
        let mut right_sum = 0.0;
        let mut right_len = 0usize;
        if downloads.active_count > 0 {
          right_sum += 50.0;
          right_len += 1;
        }
        // Downloads button is always visible.
        right_sum += button_side;
        right_len += 1;
        if loading && !is_compact {
          let font_id = egui::TextStyle::Small.resolve(ui.style());
          let stage_key = stage.filter(|s| *s != StageHeartbeat::Done);
          let label_width = ctx.data_mut(|d| {
            let cache_id = address_bar_id.with("loading_text_width_cache");
            let cache = d.get_temp_mut_or_default::<LoadingTextWidthCache>(cache_id);
            cache.width(ui, stage_key, &font_id, loading_text)
          });
          right_sum += label_width;
          right_len += 1;
        }
        if loading {
          right_sum += icon_side;
          right_len += 1;
        }
        if warn_t > 0.0 {
          right_sum += badge_side;
          right_len += 1;
        }
        if err_t > 0.0 {
          right_sum += badge_side;
          right_len += 1;
        }
        if show_try_http {
          right_sum += try_http_button_width;
          right_len += 1;
        }
        if omnibox_bookmarks.is_some() {
          right_sum += button_side;
          right_len += 1;
        }
        let right_width =
          (right_sum + item_spacing * (right_len.saturating_sub(1) as f32)).min(full_rect.width());
        let gap = if right_width > 0.0 { item_spacing } else { 0.0 };
        let left_width = (full_rect.width() - right_width - gap).max(0.0);
        let left_rect =
          egui::Rect::from_min_size(full_rect.min, egui::vec2(left_width, full_rect.height()));
        let right_rect = egui::Rect::from_min_size(
          egui::pos2(full_rect.right() - right_width, full_rect.top()),
          egui::vec2(right_width, full_rect.height()),
        );

        ui.allocate_ui_at_rect(left_rect, |ui| {
          ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            match indicator {
              security_indicator::SecurityIndicator::Secure => {
                let label = indicator.tooltip();
                let resp = icon_tinted(
                  ui,
                  BrowserIcon::LockSecure,
                  ui.spacing().icon_width,
                  ui.visuals().text_color(),
                );
                show_tooltip_on_hover_or_focus(ui, &resp, label);
                resp.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label));
              }
              security_indicator::SecurityIndicator::Insecure => {
                let label = indicator.tooltip();
                let resp = icon_tinted(
                  ui,
                  BrowserIcon::WarningInsecure,
                  ui.spacing().icon_width,
                  ui.visuals().warn_fg_color,
                );
                show_tooltip_on_hover_or_focus(ui, &resp, label);
                resp.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label));
              }
              security_indicator::SecurityIndicator::Neutral => {
                let label = indicator.tooltip();
                let resp = icon_tinted(
                  ui,
                  BrowserIcon::Info,
                  ui.spacing().icon_width,
                  ui.visuals().weak_text_color(),
                );
                show_tooltip_on_hover_or_focus(ui, &resp, label);
                resp.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label));
              }
            }

            if show_text_edit {
              // Apply focus/selection requests *before* constructing the `TextEdit` so they take
              // effect in the same frame.
              if app.chrome.request_focus_address_bar {
                ui.memory_mut(|mem| mem.request_focus(address_bar_id));
                app.chrome.request_focus_address_bar = false;
              }
              if app.chrome.request_select_all_address_bar {
                let end = app.chrome.address_bar_text.chars().count();
                let mut state =
                  egui::text_edit::TextEditState::load(ctx, address_bar_id).unwrap_or_default();
                state.set_ccursor_range(Some(egui::text::CCursorRange::two(
                  egui::text::CCursor::new(0),
                  egui::text::CCursor::new(end),
                )));
                state.store(ctx, address_bar_id);
                app.chrome.request_select_all_address_bar = false;
              }

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
            } else if active_url_trim.is_empty() {
              let max_width = ui.available_width().max(0.0);
              let placeholder_galley = ctx.data_mut(|d| {
                let cache_id = address_bar_id.with("placeholder_galley_cache");
                let cache =
                  d.get_temp_mut_or_default::<AddressBarPlaceholderGalleyCache>(cache_id);
                cache.update(ui, max_width);
                cache.galley()
              });
              ui.add(egui::Label::new(placeholder_galley));
            } else {
              let max_width = ui.available_width().max(0.0);
              let galley = ctx.data_mut(|d| {
                let cache_id = address_bar_id.with("display_galley_cache");
                let cache = d.get_temp_mut_or_default::<AddressBarDisplayGalleyCache>(cache_id);
                cache.update(
                  ui,
                  formatted_url,
                  display_path_query_fragment,
                  max_width,
                  url_generation,
                );
                cache.galley()
              });
              ui.add(egui::Label::new(galley));
            }
          });
        });

        ui.allocate_ui_at_rect(right_rect, |ui| {
          ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let downloads_panel_open = app.chrome.downloads_panel_open;
            let downloads_label_cache_id = address_bar_id.with("downloads_label_cache");
            let mut downloads_label_cache: DownloadsUiLabelCache = ctx.data_mut(|d| {
              std::mem::take(
                d.get_temp_mut_or_default::<DownloadsUiLabelCache>(downloads_label_cache_id),
              )
            });
            downloads_label_cache.update(
              downloads.active_count,
              downloads.received_bytes,
              downloads.total_bytes,
              downloads_panel_open,
            );

            {
              let downloads_toggle_label = if downloads_panel_open {
                "Hide downloads"
              } else {
                "Show downloads"
              };
              let downloads_hover = if downloads.active_count == 0 {
                downloads_toggle_label
              } else {
                downloads_label_cache.hover_label.as_str()
              };
              let downloads_progress_a11y_label = downloads_label_cache.progress_label.as_str();

              // Downloads progress indicator (optional; shown to the left of the icon).
              if downloads.active_count > 0 {
                let resp = if let Some(total) = downloads.total_bytes.filter(|t| *t > 0) {
                  let frac = (downloads.received_bytes as f32 / total as f32).clamp(0.0, 1.0);
                  ui.add(egui::ProgressBar::new(frac).desired_width(50.0).text(""))
                } else {
                  ui.add(
                    egui::ProgressBar::new(0.0)
                      .desired_width(50.0)
                      .animate(motion.enabled)
                      .text(""),
                  )
                };
                resp.widget_info({
                  let label = downloads_progress_a11y_label;
                  move || {
                    // `egui` 0.23 does not have a dedicated progress widget type, so label the widget
                    // with a descriptive `Label` for screen readers.
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, label)
                  }
                });
              }

              // Downloads button.
              let (_id, downloads_rect) = ui.allocate_space(egui::vec2(button_side, button_side));
              let downloads_id = address_bar_id.with("downloads");
              let downloads_resp = ui.interact(downloads_rect, downloads_id, egui::Sense::click());
              #[cfg(test)]
              store_test_id(ctx, "chrome_downloads_button_id", downloads_resp.id);
              show_tooltip_on_hover_or_focus(ui, &downloads_resp, downloads_hover);
              downloads_resp.widget_info({
                let label = downloads_hover;
                move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
              });

              // Expose the button as an expanded/collapsed control so screen readers can announce
              // whether the downloads side panel is open.
              let _ = ctx.accesskit_node_builder(downloads_resp.id, |builder| {
                builder.set_expanded(downloads_panel_open);
                if downloads_panel_open {
                  builder.add_action(accesskit::Action::Collapse);
                  builder.remove_action(accesskit::Action::Expand);
                } else {
                  builder.add_action(accesskit::Action::Expand);
                  builder.remove_action(accesskit::Action::Collapse);
                }
              });

              // AccessKit may request explicit expand/collapse actions when the node exposes an
              // expanded state.
              let expand_requested = ui.input(|i| {
                i.has_accesskit_action_request(downloads_resp.id, accesskit::Action::Expand)
              });
              let collapse_requested = ui.input(|i| {
                i.has_accesskit_action_request(downloads_resp.id, accesskit::Action::Collapse)
              });

              // Micro-interaction: fade a subtle hover fill in/out.
              let highlight = downloads_resp.hovered() || downloads_resp.has_focus();
              let hover_t = motion.animate_bool(
                ui.ctx(),
                downloads_id.with("hover"),
                highlight,
                motion.durations.hover_fade,
              );
              if hover_t > 0.0 {
                let rounding = egui::Rounding::same(
                  (ui.visuals().widgets.inactive.rounding.nw * 0.8).clamp(4.0, 6.0),
                );
                ui.painter().rect_filled(
                  downloads_rect,
                  rounding,
                  with_alpha(
                    ui.visuals().widgets.hovered.bg_fill.gamma_multiply(0.85),
                    hover_t,
                  ),
                );
              }

              let downloads_icon_color = if downloads.active_count > 0 || highlight {
                ui.visuals().text_color()
              } else {
                ui.visuals().weak_text_color()
              };
              paint_icon_in_rect(
                ui,
                downloads_rect,
                BrowserIcon::Download,
                ui.spacing().icon_width,
                downloads_icon_color,
              );

              if downloads.active_count > 0 {
                // Render a small count badge on the icon so multiple downloads are visible at a glance.
                let count_text = downloads_label_cache.badge_label.as_str();
                let badge_fill = ui.visuals().selection.stroke.color;
                let [r, g, b, _] = badge_fill.to_array();
                let luma = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
                let badge_text_color = if luma > 150 {
                  egui::Color32::BLACK
                } else {
                  egui::Color32::WHITE
                };

                let radius = (downloads_rect.height() * 0.23).clamp(6.0, 9.0);
                let center = egui::pos2(
                  downloads_rect.right() - radius,
                  downloads_rect.top() + radius,
                );
                ui.painter().circle_filled(center, radius, badge_fill);
                ui.painter().text(
                  center,
                  egui::Align2::CENTER_CENTER,
                  count_text,
                  egui::FontId::proportional((radius * 1.3).clamp(9.0, 12.0)),
                  badge_text_color,
                );
              }

              paint_focus_ring(ui, &downloads_resp, focus_ring);

              if (expand_requested && !downloads_panel_open)
                || (collapse_requested && downloads_panel_open)
                || downloads_resp.clicked()
                || keyboard_activate(ui, &downloads_resp)
              {
                actions.push(ChromeAction::ToggleDownloadsPanel);
              }
            }

            ctx.data_mut(|d| d.insert_temp(downloads_label_cache_id, downloads_label_cache));

            // Loading status (optional; shown to the right of downloads).
            if loading && !is_compact {
              let resp = ui.add(
                egui::Label::new(egui::RichText::new(loading_text.clone()).small())
                  .wrap(false)
                  .truncate(true),
              );
              show_tooltip_on_hover_or_focus(ui, &resp, loading_text);
            }
            if loading {
              let resp = spinner(ui, icon_side);
              show_tooltip_on_hover_or_focus(ui, &resp, loading_text);
              // In compact mode the spinner may be the only visible loading affordance, so expose the
              // full loading text to screen readers (hover text is not sufficient).
              resp.widget_info({
                let label = loading_text.clone();
                move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
              });
            }

            // Warning badge (optional; shown to the right of the spinner).
            if warn_t > 0.0 {
              let warn_fg = with_alpha(ui.visuals().warn_fg_color, warn_t);
              let warn_bg_base = egui::Color32::from_rgba_unmultiplied(
                ui.visuals().warn_fg_color.r(),
                ui.visuals().warn_fg_color.g(),
                ui.visuals().warn_fg_color.b(),
                40,
              );
              let warn_bg = with_alpha(warn_bg_base, warn_t);
              let (badge_icon, a11y_label) = warn_msg
                .and_then(|warn| crate::ui::classify_warning_toast(Some(warn)))
                .map(|presentation| {
                  let summary = presentation
                    .summary
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|summary| format!("{}: {summary}", presentation.title))
                    .unwrap_or_else(|| presentation.title.clone());
                  let icon = match presentation.icon {
                    crate::ui::WarningToastIcon::Info => BrowserIcon::Info,
                    crate::ui::WarningToastIcon::ViewportClamp => BrowserIcon::ZoomOut,
                    crate::ui::WarningToastIcon::WarningInsecure => BrowserIcon::WarningInsecure,
                  };
                  (icon, summary)
                })
                // The badge can still be visible while fading out (warn_msg already cleared).
                .unwrap_or_else(|| (BrowserIcon::Info, "Warning".to_string()));
              let resp = egui::Frame::none()
                .fill(warn_bg)
                .rounding(badge_rounding)
                .inner_margin(badge_margin)
                .show(ui, |ui| {
                  let icon_resp = icon_tinted(ui, badge_icon, ui.spacing().icon_width, warn_fg);
                  icon_resp.widget_info({
                    let label = a11y_label.clone();
                    move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
                  });
                })
                .response;
              if let Some(warn) = warn_msg {
                show_tooltip_on_hover_or_focus(ui, &resp, warn);
              }
            }

            // Error badge (optional; shown to the right of warning).
            if err_t > 0.0 {
              let err_fg = with_alpha(ui.visuals().error_fg_color, err_t);
              let err_bg_base = egui::Color32::from_rgba_unmultiplied(
                ui.visuals().error_fg_color.r(),
                ui.visuals().error_fg_color.g(),
                ui.visuals().error_fg_color.b(),
                40,
              );
              let err_bg = with_alpha(err_bg_base, err_t);
              let a11y_label = err_msg
                .map(|err| {
                  let first_line = err.lines().next().unwrap_or(err).trim();
                  if first_line.chars().count() > 160 {
                    format!(
                      "Error: {}…",
                      first_line.chars().take(160).collect::<String>()
                    )
                  } else {
                    format!("Error: {first_line}")
                  }
                })
                // The badge can still be visible while fading out (err_msg already cleared).
                .unwrap_or_else(|| "Error".to_string());
              let resp = egui::Frame::none()
                .fill(err_bg)
                .rounding(badge_rounding)
                .inner_margin(badge_margin)
                .show(ui, |ui| {
                  let icon_resp =
                    icon_tinted(ui, BrowserIcon::Error, ui.spacing().icon_width, err_fg);
                  icon_resp.widget_info({
                    let label = a11y_label.clone();
                    move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
                  });
                })
                .response;
              if let Some(err) = err_msg {
                show_tooltip_on_hover_or_focus(ui, &resp, err);
              }
            }

            // HTTPS → HTTP fallback (only shown when the active URL is https:// and the tab has an
            // error).
            if show_try_http {
              let label = "Try HTTP";
              let tooltip = "Retry this URL over HTTP";
              let err_fg = ui.visuals().error_fg_color;
              let try_http_id = address_bar_id.with("try_http");
              let (_id, rect) = ui.allocate_space(egui::vec2(try_http_button_width, button_side));
              let mut resp = ui.interact(rect, try_http_id, egui::Sense::click());
              #[cfg(test)]
              {
                store_test_id(ctx, "chrome_try_http_button_id", resp.id);
                store_test_rect(ctx, "chrome_try_http_button_rect", resp.rect);
              }
              resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
              show_tooltip_on_hover_or_focus(ui, &resp, tooltip);

              // Paint a small pill button.
              let bg_alpha = if resp.hovered() || resp.has_focus() {
                70
              } else {
                40
              };
              let err_bg =
                egui::Color32::from_rgba_unmultiplied(err_fg.r(), err_fg.g(), err_fg.b(), bg_alpha);
              ui.painter().rect_filled(rect, badge_rounding, err_bg);
              let font_id = egui::TextStyle::Small.resolve(ui.style());
              ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                font_id,
                err_fg,
              );
              paint_focus_ring(ui, &resp, focus_ring);

              if resp.clicked() || keyboard_activate(ui, &resp) {
                if let Some(http_url) = http_fallback_url_for_failed_https(active_url_trim) {
                  app.chrome.address_bar_text = http_url.clone();
                  app.chrome.address_bar_editing = false;
                  let had_focus = app.chrome.address_bar_has_focus;
                  app.chrome.address_bar_has_focus = false;
                  app.chrome.omnibox.reset();

                  actions.push(ChromeAction::NavigateTo(http_url));
                  if had_focus {
                    actions.push(ChromeAction::AddressBarFocusChanged(false));
                  }
                  resp.surrender_focus();
                }
              }
            }

            // Bookmark star (optional: only available when the caller supplies a bookmarks store).
            if omnibox_bookmarks.is_some() {
              let can_toggle = !active_url_trim.is_empty();
              let is_bookmarked = can_toggle && active_url_is_bookmarked;
              let action_label = if is_bookmarked {
                "Remove bookmark"
              } else {
                "Bookmark this page"
              };
              let tooltip = if cfg!(target_os = "macos") {
                if is_bookmarked {
                  "Remove bookmark (Cmd+D)"
                } else {
                  "Bookmark this page (Cmd+D)"
                }
              } else if is_bookmarked {
                "Remove bookmark (Ctrl+D)"
              } else {
                "Bookmark this page (Ctrl+D)"
              };
              let icon = if is_bookmarked {
                BrowserIcon::BookmarkFilled
              } else {
                BrowserIcon::BookmarkOutline
              };
              let (_id, rect) = ui.allocate_space(egui::vec2(button_side, button_side));
              let icon_id = address_bar_id.with("bookmark");
              let mut response = ui.interact(
                rect,
                icon_id,
                if can_toggle {
                  egui::Sense::click()
                } else {
                  egui::Sense::hover()
                },
              );
              #[cfg(test)]
              store_test_id(ctx, "chrome_bookmark_star_id", response.id);
              response.widget_info(move || {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, action_label)
              });
              show_tooltip_on_hover_or_focus(ui, &response, tooltip);

              // Micro-interaction: fade a subtle hover fill in/out.
              let highlight = can_toggle && (response.hovered() || response.has_focus());
              let hover_t = motion.animate_bool(
                ui.ctx(),
                icon_id.with("hover"),
                highlight,
                motion.durations.hover_fade,
              );
              if hover_t > 0.0 {
                let rounding = egui::Rounding::same(
                  (ui.visuals().widgets.inactive.rounding.nw * 0.8).clamp(4.0, 6.0),
                );
                ui.painter().rect_filled(
                  rect,
                  rounding,
                  with_alpha(
                    ui.visuals().widgets.hovered.bg_fill.gamma_multiply(0.85),
                    hover_t,
                  ),
                );
              }

              let color = if is_bookmarked {
                ui.visuals().selection.stroke.color
              } else if highlight {
                ui.visuals().text_color()
              } else {
                ui.visuals().weak_text_color()
              };
              paint_icon_in_rect(ui, rect, icon, ui.spacing().icon_width, color);
              paint_focus_ring(ui, &response, focus_ring);
              if can_toggle && (response.clicked() || keyboard_activate(ui, &response)) {
                actions.push(ChromeAction::ToggleBookmarkForActiveTab);
              }
            }
          });
        });
      });

      // Border stroke for the pill.
      let border_stroke = if bar_response.hovered() {
        ui.visuals().widgets.hovered.bg_stroke
      } else {
        ui.visuals().widgets.inactive.bg_stroke
      };
      ui.painter()
        .rect_stroke(bar_rect, bar_rounding, border_stroke);

      let has_focus =
        bar_response.has_focus() || text_edit_response.as_ref().is_some_and(|r| r.has_focus());

      // Micro-interaction: address bar focus ring animation.
      let focus_t = motion.animate_bool(
        ctx,
        address_bar_id.with("focus_ring"),
        has_focus,
        motion.durations.focus_ring,
      );
      if focus_t > 0.0 {
        let alpha = if has_focus {
          focus_t.max(0.35)
        } else {
          focus_t
        };
        let ring_color = with_alpha(focus_ring.stroke.color, alpha);
        // Keep the ring visible even when it is animating in/out (minimum 1pt stroke).
        let ring_width = (focus_ring.stroke.width - 1.0) * focus_t + 1.0;
        // Slightly expand beyond the pill's border to make focus more obvious.
        let ring_rect = bar_rect.expand((focus_ring.expand - 1.0) * focus_t + 1.0);
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
              egui::pos2(
                (track_rect.left() + w).min(track_rect.right()),
                track_rect.bottom(),
              ),
            );
            ui.painter()
              .rect_filled(rect, egui::Rounding::same(1.0), color);
          }
          Some(LoadProgressIndicator::Indeterminate) => {
            let phase = if motion.enabled {
              // Keep repainting so the indeterminate segment animates smoothly even when the worker
              // isn't emitting progress heartbeats.
              ctx.request_repaint();
              let time = ctx.input(|i| i.time) as f32;
              (time * 1.2).fract()
            } else {
              // Reduced-motion: keep the segment static (no continuous repaints).
              0.0
            };
            let seg_w = (track_rect.width() * 0.25)
              .clamp(16.0, 120.0)
              .min(track_rect.width());
            let travel = (track_rect.width() - seg_w).max(0.0);
            let x0 = track_rect.left() + travel * phase;
            let seg_rect = egui::Rect::from_min_max(
              egui::pos2(x0, track_rect.top()),
              egui::pos2(x0 + seg_w, track_rect.bottom()),
            );
            ui.painter().with_clip_rect(track_rect).rect_filled(
              seg_rect,
              egui::Rounding::same(1.0),
              color,
            );
          }
          None => {
            // Fade-out path: we no longer have a progress value, but keep the line around briefly
            // so it doesn't "pop" out of existence.
            ui.painter()
              .rect_filled(track_rect, egui::Rounding::same(1.0), color);
          }
        }
      }

      // Display mode: show the full URL on hover and when keyboard-focused.
      if !show_text_edit {
        let tooltip = if active_url_trim.is_empty() {
          "Enter URL…"
        } else {
          active_url
        };
        // Avoid `on_hover_text`: it forces the tooltip text to be owned (`'static`) and allocates
        // every frame even when the address bar isn't hovered. We only need the tooltip when the
        // pill is hovered or focused.
        show_tooltip_on_hover_or_focus(ui, &bar_response, tooltip);

        // Address bar display-mode context menu (right click / context-menu key).
        bar_response.context_menu(|ui| {
          ui.set_min_width(140.0);

          let can_copy_url = !active_url_trim.is_empty();

          let copy_url_btn = ui.add_enabled(can_copy_url, egui::Button::new("Copy URL"));
          copy_url_btn
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Copy URL"));
          if copy_url_btn.clicked() {
            ui.ctx()
              .output_mut(|o| o.copied_text = active_url_trim.to_string());
            ui.close_menu();
          }

          // The address bar already maintains a cached parse of the active URL for display; reuse
          // it here so opening the context menu doesn't re-run URL parsing every frame.
          let can_copy_domain = can_copy_url
            && matches!(
              formatted_url.security_state,
              AddressBarSecurityState::Http | AddressBarSecurityState::Https
            )
            && (!formatted_url.display_host_prefix.is_empty()
              || !formatted_url.display_host_domain.is_empty());
          let copy_domain_btn =
            ui.add_enabled(can_copy_domain, egui::Button::new("Copy domain"));
          copy_domain_btn
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Copy domain"));
          if copy_domain_btn.clicked() {
            // Match `url::Url::host_str()` behaviour for IPv6 literals (omit brackets).
            let host = formatted_url
              .display_host_domain
              .strip_prefix('[')
              .and_then(|s| s.strip_suffix(']'))
              .unwrap_or(formatted_url.display_host_domain.as_str());
            let mut out =
              String::with_capacity(formatted_url.display_host_prefix.len() + host.len());
            out.push_str(&formatted_url.display_host_prefix);
            out.push_str(host);
            ui.ctx().output_mut(|o| o.copied_text = out);
            ui.close_menu();
          }
        });
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

        // Note: focus/select-all requests are applied before constructing the TextEdit above.

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

          let input = app.chrome.address_bar_text.as_str();
          let suggestions = {
            let ctx = OmniboxContext {
              open_tabs: &app.tabs,
              closed_tabs: &app.closed_tabs,
              visited: &app.visited,
              active_tab_id: app.active_tab_id(),
              bookmarks: omnibox_bookmarks,
              remote_search_suggest: Some(&app.chrome.remote_search_cache),
            };
            build_omnibox_suggestions_default_limit(&ctx, input)
          };
          app.chrome.omnibox.suggestions = suggestions;
          // Keep the allocation for the last-built input around: omnibox typing is a hot path.
          app
            .chrome
            .omnibox
            .last_built_for_input
            .clone_from(&app.chrome.address_bar_text);
          app.chrome.omnibox.last_built_remote_fetched_at =
            app.chrome.remote_search_cache.fetched_at;
          if app.chrome.omnibox.suggestions.is_empty() {
            app.chrome.omnibox.open = false;
          }
        }

        if has_focus && (key_arrow_down || key_arrow_up) {
          // Avoid rebuilding suggestions if we already have suggestions for the current input (for
          // example, after pressing Escape to close the dropdown while keeping focus).
          if !app.chrome.omnibox.open
            && (app.chrome.omnibox.last_built_for_input != app.chrome.address_bar_text
              || app.chrome.omnibox.suggestions.is_empty())
          {
            let input = app.chrome.address_bar_text.as_str();
            let suggestions = {
              let ctx = OmniboxContext {
                open_tabs: &app.tabs,
                closed_tabs: &app.closed_tabs,
                visited: &app.visited,
                active_tab_id: app.active_tab_id(),
                bookmarks: omnibox_bookmarks,
                remote_search_suggest: Some(&app.chrome.remote_search_cache),
              };
              build_omnibox_suggestions_default_limit(&ctx, input)
            };
            app.chrome.omnibox.suggestions = suggestions;
            app
              .chrome
              .omnibox
              .last_built_for_input
              .clone_from(&app.chrome.address_bar_text);
            app.chrome.omnibox.last_built_remote_fetched_at =
              app.chrome.remote_search_cache.fetched_at;
          }

          if app.chrome.omnibox.suggestions.is_empty() {
            app.chrome.omnibox.open = false;
          }

          let nav_key = if key_arrow_down {
            OmniboxNavKey::ArrowDown
          } else {
            OmniboxNavKey::ArrowUp
          };
          let _ = apply_omnibox_nav_key(app, nav_key);
        }

        // If remote suggestions arrived for the current query, rebuild the suggestion list so the
        // dropdown updates even when the user pauses typing.
        let omnibox_query = app
          .chrome
          .omnibox
          .original_input
          .as_deref()
          .unwrap_or(app.chrome.address_bar_text.as_str());
        if has_focus
          && app.chrome.address_bar_editing
          && app.chrome.omnibox.open
          && app.chrome.omnibox.selected.is_none()
          && app.chrome.omnibox.last_built_for_input == omnibox_query
        {
          let remote = &app.chrome.remote_search_cache;
          if remote.fetched_at != app.chrome.omnibox.last_built_remote_fetched_at {
            let remote_is_for_current_query = resolve_omnibox_search_query(omnibox_query)
              .is_some_and(|q| q == remote.query.as_str());

            if remote_is_for_current_query {
              let input = omnibox_query;
              let suggestions = {
                let ctx = OmniboxContext {
                  open_tabs: &app.tabs,
                  closed_tabs: &app.closed_tabs,
                  visited: &app.visited,
                  active_tab_id: app.active_tab_id(),
                  bookmarks: omnibox_bookmarks,
                  remote_search_suggest: Some(remote),
                };
                build_omnibox_suggestions_default_limit(&ctx, input)
              };
              app.chrome.omnibox.suggestions = suggestions;
              if let Some(original) = app.chrome.omnibox.original_input.as_ref() {
                app.chrome.omnibox.last_built_for_input.clone_from(original);
              } else {
                app
                  .chrome
                  .omnibox
                  .last_built_for_input
                  .clone_from(&app.chrome.address_bar_text);
              }
              if app.chrome.omnibox.suggestions.is_empty() {
                app.chrome.omnibox.open = false;
              }
            }

            // Either way, mark the remote cache as observed so we don't keep rebuilding every frame
            // for unrelated queries.
            app.chrome.omnibox.last_built_remote_fetched_at = remote.fetched_at;
          }
        }

        // Inline omnibox autocomplete ("ghost text") when the dropdown is open.
        //
        // We only do this when `omnibox.selected == None` so we don't fight the existing
        // arrow-key preview behaviour (which also uses `omnibox.original_input`).
        if has_focus
          && app.chrome.address_bar_editing
          && app.chrome.omnibox.open
          && app.chrome.omnibox.selected.is_none()
          && !app.chrome.omnibox.suggestions.is_empty()
          && !key_escape
        {
          let completed_len = app.chrome.address_bar_text.chars().count();
          let mut state =
            egui::text_edit::TextEditState::load(ctx, address_bar_id).unwrap_or_default();
          let range = state.ccursor_range();

          let (mut typed_len, completion_selection_active) =
            if let Some(original) = app.chrome.omnibox.original_input.as_deref() {
              let typed_len = original.chars().count();
              let selection_matches = range.is_some_and(|range| {
                range.primary.index == typed_len && range.secondary.index == completed_len
              });
              let prefix_matches =
                ascii_starts_with_case_insensitive(app.chrome.address_bar_text.as_str(), original);
              (
                typed_len,
                typed_len < completed_len && selection_matches && prefix_matches,
              )
            } else {
              (completed_len, false)
            };

          // Accept inline completion on Tab (optional; Right Arrow is handled naturally by TextEdit).
          if completion_selection_active && key_tab {
            app.chrome.omnibox.original_input = None;
            state.set_ccursor_range(Some(egui::text::CCursorRange::one(
              egui::text::CCursor::new(completed_len),
            )));
            state.store(ctx, address_bar_id);
          } else {
            // If the user moved the caret/selection away from the inline-completion suffix (e.g.
            // Right Arrow, mouse click), stop treating it as an active completion so Escape doesn't
            // unexpectedly restore old input.
            if app.chrome.omnibox.original_input.is_some() && !completion_selection_active {
              app.chrome.omnibox.original_input = None;
              typed_len = completed_len;
            }

            let cursor_at_typed_end = range.is_some_and(|range| {
              range.primary.index == typed_len && range.secondary.index == typed_len
            });
            let should_try_completion = completion_selection_active || cursor_at_typed_end;

            if should_try_completion && typed_len > 0 {
              let candidate = if let Some(original) = app.chrome.omnibox.original_input.as_deref() {
                app
                  .chrome
                  .omnibox
                  .suggestions
                  .iter()
                  .filter_map(omnibox_suggestion_fill_text)
                  .find(|fill| {
                    fill.chars().count() > typed_len
                      && ascii_starts_with_case_insensitive(fill, original)
                  })
              } else {
                // Borrow the current input only long enough to choose a completion candidate; we may
                // move the string into `omnibox.original_input` below.
                let typed = app.chrome.address_bar_text.as_str();
                app
                  .chrome
                  .omnibox
                  .suggestions
                  .iter()
                  .filter_map(omnibox_suggestion_fill_text)
                  .find(|fill| {
                    fill.chars().count() > typed_len
                      && ascii_starts_with_case_insensitive(fill, typed)
                  })
              };

              if let Some(candidate) = candidate {
                let candidate_len = candidate.chars().count();

                if app.chrome.omnibox.original_input.is_none() {
                  // Preserve the user-typed input so Escape can restore it.
                  app.chrome.omnibox.original_input =
                    Some(std::mem::take(&mut app.chrome.address_bar_text));
                }

                if app.chrome.address_bar_text != candidate {
                  app.chrome.address_bar_text = candidate.to_string();
                }

                state.set_ccursor_range(Some(egui::text::CCursorRange::two(
                  egui::text::CCursor::new(typed_len),
                  egui::text::CCursor::new(candidate_len),
                )));
                state.store(ctx, address_bar_id);
                ctx.request_repaint();
              } else if completion_selection_active {
                // Suggestions no longer extend the typed prefix: drop the stale completion.
                if let Some(original) = app.chrome.omnibox.original_input.take() {
                  app.chrome.address_bar_text = original;
                  state.set_ccursor_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(typed_len),
                  )));
                  state.store(ctx, address_bar_id);
                  ctx.request_repaint();
                }
              }
            }
          }
        }

        if has_focus && key_escape {
          let was_open_or_selected =
            app.chrome.omnibox.open || app.chrome.omnibox.selected.is_some();
          let _ = apply_omnibox_nav_key(app, OmniboxNavKey::Escape);
          if was_open_or_selected {
            // Ensure we paint at least one follow-up frame so the dropdown can fade out smoothly.
            let end = app.chrome.address_bar_text.chars().count();
            let mut state =
              egui::text_edit::TextEditState::load(ctx, address_bar_id).unwrap_or_default();
            state.set_ccursor_range(Some(egui::text::CCursorRange::one(
              egui::text::CCursor::new(end),
            )));
            state.store(ctx, address_bar_id);
            ctx.request_repaint();
          } else {
            response.surrender_focus();
            actions.push(ChromeAction::AddressBarFocusChanged(false));
          }
        }

        if has_focus && (key_enter || key_alt_enter) {
          let outcome = apply_omnibox_nav_key(app, OmniboxNavKey::Enter);
          if let Some(resolved_action) = outcome.action {
            // Alt+Enter should open the resolved navigation in a new foreground tab, leaving the
            // current tab unchanged. (If the selected suggestion is an open-tab activation, keep the
            // existing behaviour.)
            let action = if key_alt_enter {
              match resolved_action {
                ChromeAction::NavigateTo(url) => ChromeAction::OpenUrlInNewTab(url),
                other => other,
              }
            } else {
              resolved_action
            };

            actions.push(action);
            actions.push(ChromeAction::AddressBarFocusChanged(false));
            response.surrender_focus();
          }
        }

        // Advertise the address bar as a combobox-like control so assistive tech can associate the
        // omnibox suggestions list with the focused input.
        let omnibox_expanded =
          app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty();
        let _ = ctx.accesskit_node_builder(response.id, |builder| {
          builder.set_role(accesskit::Role::SearchBox);
          builder.set_expanded(omnibox_expanded);
        });

        address_bar_text_edit_response = Some(response.clone());
      }

      // Toolbar menu (hamburger) button.
      //
      // We can't use `ui.menu_button` here because it only accepts text, but we want to use the
      // repo-owned SVG icon set for consistent chrome iconography.
      //
      // NOTE: This is rendered *after* the address bar so Tab/Shift+Tab focus traversal matches the
      // left-to-right visual order.
      let menu_id = egui::Id::new("chrome_menu");
      let menu_open_id = menu_id.with("open");
      let menu_popup_id = menu_id.with("popup");
      let menu_opener_id = menu_id.with("opener_id");
      let menu_focus_ids_id = menu_id.with("popup_focus_ids");
      let mut menu_open = ctx
        .data(|d| d.get_temp::<bool>(menu_open_id))
        .unwrap_or(false);
      let menu_open_prev = menu_open;

      let menu_button =
        icon_button_with_id(ui, menu_id.with("button"), BrowserIcon::Menu, "Menu", true);
      #[cfg(test)]
      store_test_rect(ctx, "chrome_menu_button_rect", menu_button.rect);
      #[cfg(test)]
      store_test_id(ctx, "chrome_menu_button_id", menu_button.id);

      let menu_clicked = menu_button.clicked();
      if menu_clicked {
        menu_open = !menu_open;
      }
      let menu_opened_now = menu_clicked && menu_open;
      if menu_opened_now {
        ctx.data_mut(|d| d.insert_temp(menu_opener_id, Some(menu_button.id)));
      }

      let open_t = motion.animate_bool(
        ctx,
        menu_id.with("popup_open"),
        menu_open,
        motion.durations.popup_open,
      );
      let open_opacity = open_t.clamp(0.0, 1.0);

      let mut menu_rect: Option<egui::Rect> = None;
      let mut popup_focus_ids: Vec<egui::Id> = Vec::new();
      let mut closed_by_click_outside = false;
      if !shortcuts_enabled && menu_open {
        // When a modal dialog is visible, treat chrome popups as inactive and close them without
        // restoring focus to the opener (the modal owns focus).
        closed_by_click_outside = true;
        menu_open = false;
      }
      if menu_open || open_opacity > 0.0 {
        let mut close_menu = false;
        // Anchor the popup menu below the menu button.
        let pos = egui::pos2(menu_button.rect.left(), menu_button.rect.bottom());
        let area = egui::Area::new(menu_popup_id)
          .order(egui::Order::Foreground)
          .fixed_pos(pos)
          .constrain_to(ctx.screen_rect())
          .interactable(menu_open && shortcuts_enabled);
        let inner = area.show(ctx, |ui| {
          ui.set_enabled(menu_open && shortcuts_enabled);
          ui.visuals_mut().override_text_color =
            Some(with_alpha(ui.visuals().text_color(), open_opacity));
          let mut frame = egui::Frame::popup(ui.style());
          frame.fill = with_alpha(frame.fill, open_opacity);
          frame.stroke.color = with_alpha(frame.stroke.color, open_opacity);
          frame.shadow.color = with_alpha(frame.shadow.color, open_opacity);
          frame.show(ui, |ui| {
            ui.set_min_width(220.0);

            if shortcuts_enabled
              && menu_open
              && ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape))
            {
              close_menu = true;
            }

            ui.label(egui::RichText::new("Bookmarks").strong());
            let toggle_bookmark_label = if active_url_is_bookmarked {
              "Remove bookmark"
            } else {
              "Bookmark this page"
            };
            let toggle_bookmark = ui.add_enabled(
              !active_url_trim.is_empty(),
              egui::Button::new(toggle_bookmark_label),
            );
            popup_focus_ids.push(toggle_bookmark.id);
            toggle_bookmark.widget_info({
              let label = toggle_bookmark_label;
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
            });
            if menu_opened_now && !active_url_trim.is_empty() {
              toggle_bookmark.request_focus();
            }
            if menu_open {
              #[cfg(test)]
              store_test_rect(
                ctx,
                "chrome_menu_item_toggle_bookmark_rect",
                toggle_bookmark.rect,
              );
            }
            if menu_open && toggle_bookmark.clicked() {
              actions.push(ChromeAction::ToggleBookmarkForActiveTab);
              close_menu = true;
            }

            let mut show_bookmarks_manager = app.chrome.bookmarks_manager_open;
            let bookmarks_mgr = ui.checkbox(&mut show_bookmarks_manager, "Bookmarks manager");
            // Note: `egui::Checkbox` mutates the bound boolean on click. Use the post-interaction
            // value (`show_bookmarks_manager`) so the a11y label stays consistent with the exposed
            // checked state for the current frame.
            let bookmarks_mgr_a11y_label = if show_bookmarks_manager {
              "Hide bookmarks manager"
            } else {
              "Show bookmarks manager"
            };
            popup_focus_ids.push(bookmarks_mgr.id);
            bookmarks_mgr.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, bookmarks_mgr_a11y_label)
            });
            if menu_opened_now && active_url_trim.is_empty() {
              bookmarks_mgr.request_focus();
            }
            if menu_open {
              #[cfg(test)]
              store_test_rect(
                ctx,
                "chrome_menu_item_toggle_bookmarks_manager_rect",
                bookmarks_mgr.rect,
              );
            }
            if menu_open && bookmarks_mgr.clicked() {
              actions.push(ChromeAction::ToggleBookmarksManager);
              close_menu = true;
            }

            ui.separator();

            ui.label(egui::RichText::new("History").strong());
            let mut show_history_panel = app.chrome.history_panel_open;
            let history = ui.checkbox(&mut show_history_panel, "History panel");
            // Keep the a11y label consistent with the checkbox state for the current frame.
            let history_a11y_label = if show_history_panel {
              "Hide history panel"
            } else {
              "Show history panel"
            };
            popup_focus_ids.push(history.id);
            history.widget_info(move || {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, history_a11y_label)
            });
            if menu_open {
              #[cfg(test)]
              store_test_rect(ctx, "chrome_menu_item_toggle_history_rect", history.rect);
            }
            if menu_open && history.clicked() {
              actions.push(ChromeAction::ToggleHistoryPanel);
              close_menu = true;
            }

            let clear = ui.button("Clear browsing data…");
            popup_focus_ids.push(clear.id);
            clear.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear browsing data")
            });
            if menu_open {
              #[cfg(test)]
              store_test_rect(
                ctx,
                "chrome_menu_item_open_clear_browsing_data_rect",
                clear.rect,
              );
            }
            if menu_open && clear.clicked() {
              actions.push(ChromeAction::OpenClearBrowsingDataDialog);
              close_menu = true;
            }

            ui.separator();

            ui.label(egui::RichText::new("Settings").strong());
            let home_page = ui.button("Set home page…");
            popup_focus_ids.push(home_page.id);
            home_page
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Set home page"));
            if menu_open {
              #[cfg(test)]
              store_test_rect(
                ctx,
                "chrome_menu_item_open_home_url_dialog_rect",
                home_page.rect,
              );
            }
            if menu_open && home_page.clicked() {
              actions.push(ChromeAction::OpenHomeUrlDialog);
              close_menu = true;
            }

            ui.separator();

            ui.label(egui::RichText::new("Window").strong());
            let mut show_menu_bar = app.chrome.show_menu_bar;
            let mut show_menu_bar_toggle = ui.checkbox(&mut show_menu_bar, "Show menu bar");
            if let Some(forced) = show_menu_bar_env_override() {
              show_menu_bar_toggle = show_menu_bar_toggle.on_hover_text(if forced {
                "FASTR_BROWSER_SHOW_MENU_BAR is set; the menu bar is forced shown for this process. This checkbox controls the persisted session preference used when the env override is not set."
              } else {
                "FASTR_BROWSER_SHOW_MENU_BAR is set; the menu bar is forced hidden for this process. This checkbox controls the persisted session preference used when the env override is not set."
              });
            }
            popup_focus_ids.push(show_menu_bar_toggle.id);
            show_menu_bar_toggle.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, "Show menu bar")
            });
            if menu_open {
              #[cfg(test)]
              store_test_rect(
                ctx,
                "chrome_menu_item_toggle_menu_bar_rect",
                show_menu_bar_toggle.rect,
              );
            }
            if menu_open && show_menu_bar_toggle.clicked() {
              actions.push(ChromeAction::SetShowMenuBar(show_menu_bar));
              close_menu = true;
            }
          })
        });
        menu_rect = Some(inner.response.rect);
        if close_menu {
          menu_open = false;
        }
      }

      if !popup_focus_ids.is_empty() {
        ctx.data_mut(|d| {
          d.insert_temp(menu_focus_ids_id, popup_focus_ids.clone());
        });
      }

      // Best-effort: close the menu when clicking outside the popup and button.
      if menu_open {
        let clicked_outside = ctx.input(|i| {
          i.pointer.any_pressed()
            && i
              .pointer
              .interact_pos()
              .or_else(|| i.pointer.latest_pos())
              .is_some_and(|pos| {
                !menu_button.rect.contains(pos) && menu_rect.is_some_and(|rect| !rect.contains(pos))
              })
        });
        if clicked_outside {
          closed_by_click_outside = true;
          menu_open = false;
        }
      }
      if menu_open_prev && !menu_open {
        let opener = ctx
          .data(|d| d.get_temp::<Option<egui::Id>>(menu_opener_id))
          .unwrap_or(None);
        let focus_ids = ctx
          .data(|d| d.get_temp::<Vec<egui::Id>>(menu_focus_ids_id))
          .unwrap_or_default();
        restore_focus_or_clear_popup_focus(ctx, !closed_by_click_outside, opener, &focus_ids);

        // Ensure we paint at least one follow-up frame so the menu can fade out smoothly.
        ctx.request_repaint();
      }

      ctx.data_mut(|d| {
        d.insert_temp(menu_open_id, menu_open);
      });

      let appearance_response = icon_button_with_id(
        ui,
        egui::Id::new("chrome_appearance_button"),
        BrowserIcon::Appearance,
        "Appearance",
        true,
      );
      #[cfg(test)]
      store_test_id(ctx, "chrome_appearance_button_id", appearance_response.id);
      appearance_button_rect = Some(appearance_response.rect);
      // AccessKit may request explicit expand/collapse actions when the node exposes an expanded
      // state. Use those (rather than the default action) so screen readers can open/close the
      // popup with consistent semantics.
      let expand_requested = ui.input(|i| {
        i.has_accesskit_action_request(appearance_response.id, accesskit::Action::Expand)
      });
      let collapse_requested = ui.input(|i| {
        i.has_accesskit_action_request(appearance_response.id, accesskit::Action::Collapse)
      });

      if expand_requested || collapse_requested {
        let desired_open = expand_requested && !collapse_requested;
        if app.chrome.appearance_popup_open != desired_open {
          app.chrome.appearance_popup_open = desired_open;
          appearance_opened_now = desired_open;
          if desired_open {
            // Record the opener so we can restore focus when the popup closes.
            ctx.data_mut(|d| {
              d.insert_temp(
                egui::Id::new("fastr_appearance_popup").with("opener_id"),
                Some(appearance_response.id),
              );
            });
          } else {
            // Closing via AccessKit does not necessarily move egui focus. Explicitly restore focus
            // to the opener to avoid leaving focus on hidden popup widgets.
            appearance_response.request_focus();
            ctx.request_repaint();
          }
        }
      } else if appearance_response.clicked() {
        app.chrome.appearance_popup_open = !app.chrome.appearance_popup_open;
        appearance_opened_now = app.chrome.appearance_popup_open;
        if appearance_opened_now {
          // Record the opener so we can restore focus when the popup closes.
          ctx.data_mut(|d| {
            d.insert_temp(
              egui::Id::new("fastr_appearance_popup").with("opener_id"),
              Some(appearance_response.id),
            );
          });
        }
      }

      // Expose expanded state + expand/collapse actions to assistive tech (AccessKit) so screen
      // readers can announce and toggle the open/closed state of the popup.
      let _ = ctx.accesskit_node_builder(appearance_response.id, |builder| {
        builder.set_expanded(app.chrome.appearance_popup_open);
        if app.chrome.appearance_popup_open {
          builder.add_action(accesskit::Action::Collapse);
          builder.remove_action(accesskit::Action::Expand);
        } else {
          builder.add_action(accesskit::Action::Expand);
          builder.remove_action(accesskit::Action::Collapse);
        }
      });
    });

    // ---------------------------------------------------------------------------
    // Find in page bar (Ctrl/Cmd+F)
    // ---------------------------------------------------------------------------
    if open_find_in_page {
      if let Some(tab) = app.active_tab_mut() {
        tab.find.open = true;
      }
    }

    if let Some(tab) = app.active_tab_mut() {
      if tab.find.open {
        let tab_id = tab.id;

        ui.separator();
        ui.horizontal(|ui| {
          ui.spacing_mut().item_spacing.x = 8.0;
          ui.label(egui::RichText::new("Find").strong());

          let find_id = ui.make_persistent_id(("find_bar_input", tab_id));
          let egui_focus = ctx.memory(|mem| mem.has_focus(find_id));
          let show_text_edit = egui_focus || open_find_in_page;

          // Apply focus/select-all requests before building the `TextEdit` so Ctrl/Cmd+F followed by
          // immediate typing works reliably (no extra frame required).
          if open_find_in_page {
            ui.memory_mut(|mem| mem.request_focus(find_id));
            let end = tab.find.query.chars().count();
            let mut state = egui::text_edit::TextEditState::load(ctx, find_id).unwrap_or_default();
            state.set_ccursor_range(Some(egui::text::CCursorRange::two(
              egui::text::CCursor::new(0),
              egui::text::CCursor::new(end),
            )));
            state.store(ctx, find_id);
          }

          // Consume keyboard actions (Enter/Escape) when the find bar is focused so they don't leak
          // to page input.
          let mut key_enter = false;
          let mut key_shift_enter = false;
          let mut key_escape = false;
          if show_text_edit && shortcuts_enabled {
            ui.input_mut(|i| {
              key_shift_enter = i.consume_key(
                egui::Modifiers {
                  shift: true,
                  ..Default::default()
                },
                egui::Key::Enter,
              );
              key_enter = i.consume_key(Default::default(), egui::Key::Enter);
              key_escape = i.consume_key(Default::default(), egui::Key::Escape);
            });
          }

          let response = ui.add(
            egui::TextEdit::singleline(&mut tab.find.query)
              .id(find_id)
              .desired_width(220.0)
              .hint_text("Find in page…"),
          );
          response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, a11y::FIND_IN_PAGE_LABEL)
          });

          let match_count = tab.find.match_count;
          let active_idx = tab.find.active_match_index.map(|i| i + 1).unwrap_or(0);
          {
            let cache_id = find_id.with("match_label_cache");
            let mut cache: FindInPageMatchLabelCache = ctx.data_mut(|d| {
              std::mem::take(d.get_temp_mut_or_default::<FindInPageMatchLabelCache>(cache_id))
            });
            ui.label(cache.label(active_idx, match_count));
            ctx.data_mut(|d| d.insert_temp(cache_id, cache));
          }

          let prev_enabled = !tab.find.query.trim().is_empty() && match_count > 0;
          let next_enabled = prev_enabled;

          let prev_resp = icon_button(
            ui,
            BrowserIcon::ArrowUp,
            "Previous match (Shift+Enter)",
            prev_enabled,
          );
          prev_resp
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Previous match"));
          if prev_resp.clicked() {
            actions.push(ChromeAction::FindPrev(tab_id));
          }
          let next_resp = icon_button(
            ui,
            BrowserIcon::ArrowDown,
            "Next match (Enter)",
            next_enabled,
          );
          next_resp
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Next match"));
          if next_resp.clicked() {
            actions.push(ChromeAction::FindNext(tab_id));
          }

          if key_shift_enter && prev_enabled {
            actions.push(ChromeAction::FindPrev(tab_id));
          } else if key_enter && next_enabled {
            actions.push(ChromeAction::FindNext(tab_id));
          }

          let case_toggle = ui
            .toggle_value(&mut tab.find.case_sensitive, "Aa")
            .on_hover_text("Case sensitive");
          case_toggle
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Case sensitive"));
          let case_changed = case_toggle.changed();

          if response.changed() || case_changed {
            actions.push(ChromeAction::FindQuery {
              tab_id,
              query: tab.find.query.clone(),
              case_sensitive: tab.find.case_sensitive,
            });
          }

          let close_resp = icon_button(ui, BrowserIcon::Close, "Close (Esc)", true);
          close_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close find in page")
          });
          let close_clicked = close_resp.clicked();
          if close_clicked || (key_escape && show_text_edit) {
            tab.find = crate::ui::browser_app::FindInPageState::default();
            actions.push(ChromeAction::CloseFindInPage(tab_id));
            response.surrender_focus();
          }
        });
      }
    }

    if app.chrome.bookmarks_bar_visible {
      if let Some(bookmarks) = omnibox_bookmarks {
        let mut has_any = false;
        for id in &bookmarks.roots {
          if matches!(
            bookmarks.nodes.get(id),
            Some(crate::ui::BookmarkNode::Bookmark(_))
          ) {
            has_any = true;
            break;
          }
        }
        if has_any {
          ui.separator();
          let bar = bookmarks_bar_ui(ui, bookmarks, BOOKMARKS_BAR_MAX_ITEMS);
          if let Some(url) = bar.navigate_to {
            if bar.navigate_new_tab {
              actions.push(ChromeAction::NewTab);
            }
            actions.push(ChromeAction::NavigateTo(url));
          }
          if let Some(order) = bar.reorder_roots {
            actions.push(ChromeAction::ReorderBookmarksBar(order));
          }
        }
      }
    }
  });

  // -----------------------------------------------------------------------------
  // Omnibox dropdown overlay
  // -----------------------------------------------------------------------------
  let omnibox_dropdown_id = egui::Id::new("omnibox_dropdown");
  let omnibox_open_t = motion.animate_bool(
    ctx,
    omnibox_dropdown_id.with("popup_open"),
    app.chrome.address_bar_has_focus && app.chrome.omnibox.open,
    motion.durations.popup_open,
  );
  let omnibox_open_opacity = omnibox_open_t.clamp(0.0, 1.0);
  if app.chrome.address_bar_has_focus
    && !app.chrome.omnibox.suggestions.is_empty()
    && (app.chrome.omnibox.open || omnibox_open_opacity > 0.0)
  {
    if let Some(anchor) = address_bar_rect {
      let pos = egui::pos2(anchor.min.x, anchor.max.y);
      let area = egui::Area::new(omnibox_dropdown_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .constrain_to(ctx.screen_rect())
        .interactable(app.chrome.omnibox.open && shortcuts_enabled);

      let mut clicked_suggestion: Option<usize> = None;
      let inner = area.show(ctx, |ui| {
        ui.set_enabled(app.chrome.omnibox.open && shortcuts_enabled);
        ui.visuals_mut().override_text_color =
          Some(with_alpha(ui.visuals().text_color(), omnibox_open_opacity));
        let mut frame = egui::Frame::popup(ui.style());
        frame.fill = with_alpha(frame.fill, omnibox_open_opacity);
        frame.stroke.color = with_alpha(frame.stroke.color, omnibox_open_opacity);
        frame.shadow.color = with_alpha(frame.shadow.color, omnibox_open_opacity);
        frame.show(ui, |ui| {
          let width = anchor.width();
          if width.is_finite() && width > 0.0 {
            ui.set_min_width(width);
            ui.set_max_width(width);
          }

          const MAX_VISIBLE_ROWS: usize = 8;
          let row_height = ui.spacing().interact_size.y.max(24.0);
          let max_height = row_height * (MAX_VISIBLE_ROWS as f32);
 
          let a11y_label_cache_id = omnibox_dropdown_id.with("a11y_label_cache");
          let mut a11y_label_cache: OmniboxSuggestionA11yLabelCache = ctx.data_mut(|d| {
            std::mem::take(
              d.get_temp_mut_or_default::<OmniboxSuggestionA11yLabelCache>(a11y_label_cache_id),
            )
          });

          let suggestions_list = egui::ScrollArea::vertical()
            .max_height(max_height)
            .show(ui, |ui| {
              let scroll_selected_id = omnibox_dropdown_id.with("scroll_selected");
              let mut scrolled_to_selected = ctx
                .data(|d| d.get_temp::<Option<usize>>(scroll_selected_id))
                .unwrap_or(None);
              if app.chrome.omnibox.selected.is_none() {
                scrolled_to_selected = None;
              }
              let dummy_row_id = omnibox_dropdown_id.with("a11y_label_used_rows");
              let mut used_row_ids = [dummy_row_id; 16];
              let mut used_row_ids_len = 0usize;
              for (idx, suggestion) in app.chrome.omnibox.suggestions.iter().enumerate() {
                let is_selected = app.chrome.omnibox.selected == Some(idx);
                // Use a deterministic row id so egui/AccessKit node ids stay stable across frames,
                // even when the suggestion list is rebuilt (insert/remove/reorder).
                let row_id = match &suggestion.action {
                  OmniboxAction::NavigateToUrl => omnibox_dropdown_id.with((
                    "row",
                    "navigate",
                    suggestion.url.as_deref(),
                    suggestion.source,
                    suggestion.title.as_deref(),
                  )),
                  OmniboxAction::Search(query) => {
                    omnibox_dropdown_id.with(("row", "search", query, suggestion.source))
                  }
                  OmniboxAction::ActivateTab(tab_id) => {
                    omnibox_dropdown_id.with(("row", "tab", tab_id))
                  }
                };
                if used_row_ids_len < used_row_ids.len() {
                  used_row_ids[used_row_ids_len] = row_id;
                  used_row_ids_len += 1;
                }
 
                let (_auto_id, rect) =
                  ui.allocate_space(egui::vec2(ui.available_width(), row_height));
                let response = ui.interact(rect, row_id, egui::Sense::click());
                response.widget_info({
                  let label = a11y_label_cache.label(row_id, suggestion);
                  move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
                });
                let _ = ctx.accesskit_node_builder(row_id, |builder| {
                  builder.set_role(accesskit::Role::ListBoxOption);
                  builder.set_selected(is_selected);
                });
 
                let hover_t = motion.animate_bool(
                  ctx,
                  row_id.with("hover"),
                  response.hovered(),
                  motion.durations.hover_fade,
                );
                let selected_t = motion.animate_bool(
                  ctx,
                  row_id.with("selected"),
                  is_selected,
                  motion.durations.hover_fade,
                );
                if hover_t > 0.0 {
                  ui.painter().rect_filled(
                    rect,
                    0.0,
                    with_alpha(
                      ui.visuals().widgets.hovered.bg_fill,
                      hover_t * omnibox_open_opacity,
                    ),
                  );
                }
                if selected_t > 0.0 {
                  ui.painter().rect_filled(
                    rect,
                    0.0,
                    with_alpha(
                      ui.visuals().selection.bg_fill,
                      selected_t * omnibox_open_opacity,
                    ),
                  );
                }
                if is_selected && scrolled_to_selected != Some(idx) {
                  response.scroll_to_me(Some(egui::Align::Center));
                  scrolled_to_selected = Some(idx);
                }
 
                ui.allocate_ui_at_rect(rect, |ui| {
                  ui.spacing_mut().item_spacing.x = 8.0;
                  ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    match omnibox_suggestion_icon(suggestion) {
                      OmniboxSuggestionIcon::Icon(icon) => {
                        // Render decorative row icons without allocating an egui widget so we don't
                        // add noise to the accessibility tree (the row itself has a semantic label).
                        let icon_side = ui.spacing().icon_width;
                        let (_id, icon_rect) =
                          ui.allocate_space(egui::vec2(icon_side, row_height));
                        paint_icon_in_rect(
                          ui,
                          icon_rect,
                          icon,
                          icon_side,
                          with_alpha(ui.visuals().text_color(), omnibox_open_opacity),
                        );
                      }
                      OmniboxSuggestionIcon::Text(text) => {
                        ui
                          .label(egui::RichText::new(text).strong())
                          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ""));
                      }
                    }
 
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
                    let secondary = secondary.filter(|s| *s != primary);
 
                    ui.vertical(|ui| {
                      // The row has a semantic AccessKit label; avoid duplicating text nodes in the
                      // accessibility tree for its visual contents.
                      ui
                        .add(egui::Label::new(primary).wrap(false).truncate(true))
                        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ""));
                      if let Some(secondary) = secondary {
                        ui
                          .add(
                            egui::Label::new(egui::RichText::new(secondary).small().color(
                              with_alpha(ui.visuals().weak_text_color(), omnibox_open_opacity),
                            ))
                            .wrap(false)
                            .truncate(true),
                          )
                          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ""));
                      }
                    });
                  });
                });
 
                if response.clicked() {
                  clicked_suggestion = Some(idx);
                }
              }
              a11y_label_cache.retain_row_ids(&used_row_ids[..used_row_ids_len]);
              ctx.data_mut(|d| {
                d.insert_temp(scroll_selected_id, scrolled_to_selected);
                d.insert_temp(a11y_label_cache_id, a11y_label_cache);
              });
            });
          let _ = ctx.accesskit_node_builder(suggestions_list.id, |builder| {
            builder.set_role(accesskit::Role::ListBox);
            builder.set_name("Omnibox suggestions".to_string());
          });
        });
      });

      if app.chrome.omnibox.open {
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
            // Ensure we paint at least one follow-up frame so the dropdown can fade out smoothly.
            ctx.request_repaint();
          } else if shortcuts_enabled && !ctx.input(|i| i.pointer.any_pressed()) {
            // Keep keyboard focus in the address bar while the dropdown is open.
            if let Some(response) = address_bar_text_edit_response.as_ref() {
              response.request_focus();
            }
          }
        }
      }
    }
  }

  // ---------------------------------------------------------------------------
  // Appearance popup
  // ---------------------------------------------------------------------------
  let appearance_open_prev = app.chrome.appearance_popup_open;
  let mut appearance_closed_by_click_outside = false;
  if shortcuts_enabled
    && app.chrome.appearance_popup_open
    && ctx.input(|i| i.key_pressed(egui::Key::Escape))
  {
    app.chrome.appearance_popup_open = false;
  }

  let appearance_popup_id = egui::Id::new("fastr_appearance_popup");
  let appearance_opener_id = appearance_popup_id.with("opener_id");
  let appearance_focus_ids_id = appearance_popup_id.with("popup_focus_ids");
  let appearance_open_t = motion.animate_bool(
    ctx,
    appearance_popup_id.with("popup_open"),
    app.chrome.appearance_popup_open,
    motion.durations.popup_open,
  );
  let appearance_open_opacity = appearance_open_t.clamp(0.0, 1.0);

  let mut popup_focus_ids: Vec<egui::Id> = Vec::new();
  if app.chrome.appearance_popup_open || appearance_open_opacity > 0.0 {
    let Some(button_rect) = appearance_button_rect else {
      app.chrome.appearance_popup_open = false;
      return actions;
    };

    let anchor = button_rect.left_bottom() + egui::vec2(0.0, 4.0);
    let area = egui::Area::new(appearance_popup_id)
      .order(egui::Order::Foreground)
      .fixed_pos(anchor)
      .interactable(app.chrome.appearance_popup_open && shortcuts_enabled);

    let mut popup_rect: Option<egui::Rect> = None;
    let inner = area.show(ctx, |ui| {
      ui.set_enabled(app.chrome.appearance_popup_open && shortcuts_enabled);
      ui.visuals_mut().override_text_color = Some(with_alpha(
        ui.visuals().text_color(),
        appearance_open_opacity,
      ));
      let mut frame = egui::Frame::popup(ui.style());
      frame.fill = with_alpha(frame.fill, appearance_open_opacity);
      frame.stroke.color = with_alpha(frame.stroke.color, appearance_open_opacity);
      frame.shadow.color = with_alpha(frame.shadow.color, appearance_open_opacity);
      frame.show(ui, |ui| {
        ui.set_min_width(260.0);
        ui.heading("Appearance");
        ui.separator();

        ui.label("Theme");
        let first_radio = ui.radio_value(&mut app.appearance.theme, ThemeChoice::System, "System");
        let light_radio = ui.radio_value(&mut app.appearance.theme, ThemeChoice::Light, "Light");
        let dark_radio = ui.radio_value(&mut app.appearance.theme, ThemeChoice::Dark, "Dark");
        #[cfg(test)]
        store_test_id(ctx, "appearance_theme_system_radio_id", first_radio.id);
        popup_focus_ids.push(first_radio.id);
        popup_focus_ids.push(light_radio.id);
        popup_focus_ids.push(dark_radio.id);

        if appearance_opened_now {
          first_radio.request_focus();
        }

        ui.add_space(8.0);
        ui.label("Accent color");
        let env_accent_active = {
          let toggles = runtime_toggles();
          parse_browser_accent_env(toggles.get(ENV_BROWSER_ACCENT)).is_some()
        };
        if env_accent_active {
          ui.label(
            egui::RichText::new(format!("Overridden by env {ENV_BROWSER_ACCENT}"))
              .small()
              .weak(),
          );
        }

        let current_accent = app
          .appearance
          .accent_color
          .as_deref()
          .and_then(parse_hex_color);

        const ACCENT_PRESETS: [(&str, RgbaColor); 6] = [
          ("Blue", RgbaColor::new(0x3B, 0x82, 0xF6, 0xFF)),
          ("Green", RgbaColor::new(0x10, 0xB9, 0x81, 0xFF)),
          ("Purple", RgbaColor::new(0xA8, 0x55, 0xF7, 0xFF)),
          ("Orange", RgbaColor::new(0xF5, 0x9E, 0x0B, 0xFF)),
          ("Red", RgbaColor::new(0xEF, 0x44, 0x44, 0xFF)),
          ("Gray", RgbaColor::new(0x9C, 0xA3, 0xAF, 0xFF)),
        ];
        let swatch_size = egui::vec2(18.0, 18.0);
        let swatch_rounding = egui::Rounding::same(4.0);
        ui.horizontal_wrapped(|ui| {
          for (label, rgba) in ACCENT_PRESETS {
            let color = rgba.to_color32();
            let selected = current_accent == Some(rgba);
            let id = ui.make_persistent_id(("accent_swatch", label));
            let (_, rect) = ui.allocate_space(swatch_size);
            let mut resp = ui.interact(rect, id, egui::Sense::click());
            if ui.is_rect_visible(rect) {
              ui.painter().rect_filled(rect, swatch_rounding, color);
              let stroke = if selected {
                ui.visuals().selection.stroke
              } else {
                ui.visuals().widgets.noninteractive.bg_stroke
              };
              ui.painter().rect_stroke(rect, swatch_rounding, stroke);
            }
            resp.widget_info({
              let label = if selected {
                format!("Set accent color: {label} (selected)")
              } else {
                format!("Set accent color: {label}")
              };
              move || egui::WidgetInfo::selected(egui::WidgetType::Button, selected, label.clone())
            });
            resp = resp.on_hover_text(label);
            show_tooltip_on_focus(ui, &resp, label);
            paint_focus_ring(ui, &resp, focus_ring);
            popup_focus_ids.push(resp.id);
            #[cfg(test)]
            if label == "Blue" {
              store_test_id(ctx, "appearance_accent_swatch_blue_id", resp.id);
            }
            // `Sense::click` widgets don't automatically activate on keyboard (Enter/Space), so wire
            // it up explicitly for keyboard-only workflows.
            let mut choose_requested = resp.clicked();
            if resp.has_focus() {
              choose_requested |= ui.input_mut(|i| {
                i.consume_key(Default::default(), egui::Key::Enter)
                  || i.consume_key(Default::default(), egui::Key::Space)
              });
            }
            if choose_requested {
              app.appearance.accent_color = Some(format_hex_color(rgba));
            }
          }
        });

        ui.horizontal(|ui| {
          ui.label("Custom");
          let mut custom = current_accent
            .map(|c| c.to_color32())
            .unwrap_or(ui.visuals().hyperlink_color);
          let mut resp = ui
            .push_id("custom_accent_color_picker", |ui| {
              egui::color_picker::color_edit_button_srgba(
                ui,
                &mut custom,
                egui::color_picker::Alpha::BlendOrAdditive,
              )
            })
            .inner;
          resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, "Custom accent color")
          });
          popup_focus_ids.push(resp.id);
          if resp.changed() {
            app.appearance.accent_color = Some(format_hex_color(RgbaColor::from(custom)));
          }
          let tooltip = if let Some(hex) = app.appearance.accent_color.as_deref() {
            format!("Custom accent color: {hex}")
          } else {
            "Custom accent color: Default".to_string()
          };
          resp = resp.on_hover_text(tooltip.clone());
          show_tooltip_on_focus(ui, &resp, &tooltip);
          paint_focus_ring(ui, &resp, focus_ring);
          if let Some(hex) = app.appearance.accent_color.as_deref() {
            ui.monospace(hex);
          } else {
            ui.label(egui::RichText::new("Default").weak());
          }
        });

        let reset_accent = ui.button("Reset accent");
        reset_accent
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reset accent"));
        popup_focus_ids.push(reset_accent.id);
        if reset_accent.clicked() {
          app.appearance.accent_color = None;
        }
        ui.add_space(8.0);
        ui.label("UI scale");
        let ui_scale_resp = ui.add(
          egui::Slider::new(&mut app.appearance.ui_scale, MIN_UI_SCALE..=MAX_UI_SCALE)
            .clamp_to_range(true)
            .show_value(true),
        );
        ui_scale_resp
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, "UI scale"));
        popup_focus_ids.push(ui_scale_resp.id);
        let reset_scale = ui.button("Reset scale (1.0)");
        reset_scale
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reset scale (1.0)"));
        popup_focus_ids.push(reset_scale.id);
        if reset_scale.clicked() {
          app.appearance.ui_scale = DEFAULT_UI_SCALE;
        }

        ui.add_space(8.0);
        let high_contrast = ui.checkbox(&mut app.appearance.high_contrast, "High contrast");
        high_contrast
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, "High contrast"));
        let reduced_motion = ui.checkbox(&mut app.appearance.reduced_motion, "Reduced motion");
        reduced_motion
          .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, "Reduced motion"));
        popup_focus_ids.push(high_contrast.id);
        popup_focus_ids.push(reduced_motion.id);

        // Clamp/sanitize any values that could come from hand-edited session state.
        app.appearance = std::mem::take(&mut app.appearance).sanitized();
      })
    });

    popup_rect = Some(inner.response.rect);

    if app.chrome.appearance_popup_open {
      let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed()
          && i
            .pointer
            .interact_pos()
            .or_else(|| i.pointer.latest_pos())
            .is_some_and(|pos| !popup_rect.unwrap().contains(pos))
      });
      if clicked_outside {
        appearance_closed_by_click_outside = true;
        app.chrome.appearance_popup_open = false;
      }
    }
  }

  if !popup_focus_ids.is_empty() {
    ctx.data_mut(|d| {
      d.insert_temp(appearance_focus_ids_id, popup_focus_ids.clone());
    });
  }
  if appearance_open_prev && !app.chrome.appearance_popup_open {
    let opener = ctx
      .data(|d| d.get_temp::<Option<egui::Id>>(appearance_opener_id))
      .unwrap_or(None);
    let focus_ids = ctx
      .data(|d| d.get_temp::<Vec<egui::Id>>(appearance_focus_ids_id))
      .unwrap_or_default();
    restore_focus_or_clear_popup_focus(
      ctx,
      !appearance_closed_by_click_outside,
      opener,
      &focus_ids,
    );

    // The appearance popup lives outside the winit redraw loop; request a follow-up frame so the
    // fade-out animation is visible even when closing via click-away/Escape.
    ctx.request_repaint();
  }

  // ---------------------------------------------------------------------------
  // Hovered-link status
  // ---------------------------------------------------------------------------
  //
  // Hovered link URLs are displayed via a non-intrusive overlay bubble rendered by
  // `hover_status_overlay_ui` (called by the windowed browser frontend once it knows the central
  // content rect). This keeps the page viewport size stable while reclaiming the old status bar
  // height.
  // -----------------------------------------------------------------------------
  // Tab strip context menu popup
  // -----------------------------------------------------------------------------
  if let Some(open_menu) = app.chrome.open_tab_context_menu {
    let tab_id = open_menu.tab_id;
    let menu_id = egui::Id::new(("tab_context_menu", tab_id));
    let menu_focus_ids_id = menu_id.with("popup_focus_ids");
    let opened_via_keyboard_id = menu_id.with("opened_via_keyboard");
    let opened_via_keyboard = ctx
      .data(|d| d.get_temp::<bool>(opened_via_keyboard_id))
      .unwrap_or(false);
    if opened_via_keyboard {
      // One-shot: only focus the first item on the initial keyboard open so we don't steal focus
      // while the user is navigating within the menu.
      ctx.data_mut(|d| d.insert_temp(opened_via_keyboard_id, false));
    }

    // If the tab no longer exists (e.g. it was closed while the menu is open), close the menu.
    if app.tab(tab_id).is_none() {
      let focus_ids = ctx
        .data(|d| d.get_temp::<Vec<egui::Id>>(menu_focus_ids_id))
        .unwrap_or_default();
      restore_focus_or_clear_popup_focus(ctx, false, None, &focus_ids);
      app.chrome.open_tab_context_menu = None;
      app.chrome.tab_context_menu_rect = None;
    } else {
      let can_close_tabs = app.tabs.len() > 1;
      let can_reopen_closed_tab = !app.closed_tabs.is_empty();
      let can_close_tabs_to_right = app
        .tabs
        .iter()
        .position(|t| t.id == tab_id)
        .is_some_and(|idx| idx + 1 < app.tabs.len());
      let (is_pinned, tab_group) = app
        .tab(tab_id)
        .map(|tab| (tab.pinned, tab.group))
        .unwrap_or((false, None));
      let groups_in_order = {
        let mut out = Vec::new();
        for tab in &app.tabs {
          let Some(group_id) = tab.group else {
            continue;
          };
          if out.iter().any(|(id, _, _)| *id == group_id) {
            continue;
          }
          if let Some(group) = app.tab_groups.get(&group_id) {
            let title = group.title.trim();
            let label = if title.is_empty() {
              "Group".to_string()
            } else {
              title.to_string()
            };
            out.push((group_id, label, group.color));
          }
        }
        out
      };

      let menu_pos = egui::pos2(open_menu.anchor_points.0, open_menu.anchor_points.1);

      let mut popup_focus_ids: Vec<egui::Id> = Vec::new();
      let mut restore_focus_on_close = true;
      let open_t = motion.animate_bool(
        ctx,
        menu_id.with("popup_open"),
        true,
        motion.durations.popup_open,
      );
      let open_opacity = open_t.clamp(0.0, 1.0);
      let menu_response = egui::Area::new(menu_id)
        .order(egui::Order::Foreground)
        .fixed_pos(menu_pos)
        .constrain_to(ctx.screen_rect())
        .show(ctx, |ui| {
          ui.visuals_mut().override_text_color =
            Some(with_alpha(ui.visuals().text_color(), open_opacity));
          let mut frame = egui::Frame::popup(ui.style());
          frame.fill = with_alpha(frame.fill, open_opacity);
          frame.stroke.color = with_alpha(frame.stroke.color, open_opacity);
          frame.shadow.color = with_alpha(frame.shadow.color, open_opacity);
          frame.show(ui, |ui| {
            // Provide a consistent target size for hit-testing and a more browser-like look.
            ui.set_min_width(180.0);

            let reload_tab = ui.button("Reload Tab");
            popup_focus_ids.push(reload_tab.id);
            if opened_via_keyboard {
              reload_tab.request_focus();
            }
            reload_tab
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reload tab"));
            #[cfg(test)]
            store_test_id(ctx, "test_tab_context_menu_reload_id", reload_tab.id);
            if reload_tab.clicked() {
              actions.push(ChromeAction::ReloadTab(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
            }

            let duplicate_tab = ui.button("Duplicate Tab");
            popup_focus_ids.push(duplicate_tab.id);
            duplicate_tab
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Duplicate tab"));
            if duplicate_tab.clicked() {
              actions.push(ChromeAction::DuplicateTab(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
            }

            let pin_label = if is_pinned { "Unpin tab" } else { "Pin tab" };
            let pin_button = ui.button(if is_pinned { "Unpin Tab" } else { "Pin Tab" });
            popup_focus_ids.push(pin_button.id);
            pin_button.widget_info({
              let label = pin_label;
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label)
            });
            if pin_button.clicked() {
              actions.push(ChromeAction::TogglePinTab(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
            }

            if !is_pinned {
              match tab_group {
                Some(_) => {
                  let remove_from_group = ui.button("Remove from Group");
                  popup_focus_ids.push(remove_from_group.id);
                  remove_from_group.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, "Remove from group")
                  });
                  if remove_from_group.clicked() {
                    app.remove_tab_from_group(tab_id);
                    app.chrome.open_tab_context_menu = None;
                    app.chrome.tab_context_menu_rect = None;
                  }

                  let has_other_groups = groups_in_order
                    .iter()
                    .any(|(group_id, _, _)| Some(*group_id) != tab_group);
                  if has_other_groups {
                    let move_menu = ui.menu_button("Move to Group", |ui| {
                      for (group_id, title, _) in &groups_in_order {
                        if Some(*group_id) == tab_group {
                          continue;
                        }
                        let move_to_group = ui.button(title);
                        popup_focus_ids.push(move_to_group.id);
                        move_to_group.widget_info({
                          let label = format!("Move to group: {title}");
                          move || {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, label.as_str())
                          }
                        });
                        if move_to_group.clicked() {
                          app.add_tab_to_group(tab_id, *group_id);
                          app.chrome.open_tab_context_menu = None;
                          app.chrome.tab_context_menu_rect = None;
                          ui.close_menu();
                        }
                      }
                    });
                    popup_focus_ids.push(move_menu.response.id);
                    move_menu.response.widget_info(|| {
                      egui::WidgetInfo::labeled(egui::WidgetType::Button, "Move to group")
                    });
                  }
                }
                None => {
                  let add_to_new_group = ui.button("Add to New Group");
                  popup_focus_ids.push(add_to_new_group.id);
                  add_to_new_group.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, "Add to new group")
                  });
                  if add_to_new_group.clicked() {
                    app.create_group_with_tabs(&[tab_id]);
                    app.chrome.open_tab_context_menu = None;
                    app.chrome.tab_context_menu_rect = None;
                  }

                  if groups_in_order.is_empty() {
                    let resp = ui.add_enabled(false, egui::Button::new("Add to Existing Group"));
                    resp.widget_info(|| {
                      egui::WidgetInfo::labeled(egui::WidgetType::Button, "Add to existing group")
                    });
                  } else {
                    let add_menu = ui.menu_button("Add to Existing Group", |ui| {
                      for (group_id, title, _) in &groups_in_order {
                        let add_to_group = ui.button(title);
                        popup_focus_ids.push(add_to_group.id);
                        add_to_group.widget_info({
                          let label = format!("Add to group: {title}");
                          move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.as_str())
                        });
                        if add_to_group.clicked() {
                          app.add_tab_to_group(tab_id, *group_id);
                          app.chrome.open_tab_context_menu = None;
                          app.chrome.tab_context_menu_rect = None;
                          ui.close_menu();
                        }
                      }
                    });
                    popup_focus_ids.push(add_menu.response.id);
                    add_menu.response.widget_info(|| {
                      egui::WidgetInfo::labeled(egui::WidgetType::Button, "Add to existing group")
                    });
                  }
                }
              }
            }

            let close_tab = ui.add_enabled(can_close_tabs, egui::Button::new("Close Tab"));
            popup_focus_ids.push(close_tab.id);
            close_tab
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close tab"));
            if close_tab.clicked() {
              actions.push(ChromeAction::CloseTab(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
              // The invoking tab will disappear once the action is applied; avoid focusing it.
              restore_focus_on_close = false;
            }

            ui.separator();

            let close_other_tabs =
              ui.add_enabled(can_close_tabs, egui::Button::new("Close Other Tabs"));
            popup_focus_ids.push(close_other_tabs.id);
            close_other_tabs.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close other tabs")
            });
            if close_other_tabs.clicked() {
              actions.push(ChromeAction::CloseOtherTabs(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
              restore_focus_on_close = false;
            }

            let close_tabs_to_right = ui.add_enabled(
              can_close_tabs_to_right,
              egui::Button::new("Close Tabs to the Right"),
            );
            popup_focus_ids.push(close_tabs_to_right.id);
            close_tabs_to_right.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Close tabs to the right")
            });
            if close_tabs_to_right.clicked() {
              actions.push(ChromeAction::CloseTabsToRight(tab_id));
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
              restore_focus_on_close = false;
            }

            ui.separator();

            let reopen_closed_tab = ui.add_enabled(
              can_reopen_closed_tab,
              egui::Button::new("Reopen Closed Tab"),
            );
            popup_focus_ids.push(reopen_closed_tab.id);
            reopen_closed_tab.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Reopen closed tab")
            });
            if reopen_closed_tab.clicked() {
              actions.push(ChromeAction::ReopenClosedTab);
              app.chrome.open_tab_context_menu = None;
              app.chrome.tab_context_menu_rect = None;
            }
          })
        });

      if !popup_focus_ids.is_empty() {
        ctx.data_mut(|d| {
          d.insert_temp(menu_focus_ids_id, popup_focus_ids.clone());
        });
      }

      if app.chrome.open_tab_context_menu.is_none() {
        let opener = if restore_focus_on_close {
          open_menu.opener_focus.map(egui_id_from_focus_token)
        } else {
          None
        };
        restore_focus_or_clear_popup_focus(ctx, restore_focus_on_close, opener, &popup_focus_ids);
      }

      // Update the click-outside rect for the next frame.
      if app.chrome.open_tab_context_menu.is_some() {
        let rect = menu_response.response.rect;
        app.chrome.tab_context_menu_rect = Some((rect.min.x, rect.min.y, rect.max.x, rect.max.y));
      }
    }
  }

  // -----------------------------------------------------------------------------
  // Drag hovered link → address bar navigation
  // -----------------------------------------------------------------------------
  //
  // Common browser UX: drag a link from page content and drop it onto the address bar to navigate.
  //
  // This is implemented at the chrome layer (egui) using the worker-provided `hovered_url` as the
  // drag payload. We intentionally keep this minimal: if the user doesn't exceed the drag
  // threshold, we do nothing and let the worker handle the normal click navigation.
  let dropped_url: Option<String> = ctx.input(|i| {
    let mut dropped_url: Option<String> = None;
    for event in &i.events {
      match event {
        egui::Event::PointerButton {
          pos,
          button: egui::PointerButton::Primary,
          pressed: true,
          ..
        } => {
          // Avoid treating address bar interactions as link drags even if the worker hover state is
          // stale (e.g. pointer moved to chrome without the worker receiving a hover-clear update).
          let pressed_over_address_bar = address_bar_rect.is_some_and(|r| r.contains(*pos));
          if pressed_over_address_bar {
            app.chrome.clear_link_drag();
            continue;
          }

          // Only clone the hovered URL when the user actually initiates a potential drag.
          let hovered_url = app.active_tab().and_then(|t| t.hovered_url.clone());
          if let Some(url) = hovered_url {
            app.chrome.link_drag_url = Some(url);
            app.chrome.link_drag_start_pos = Some((pos.x, pos.y));
            app.chrome.link_drag_active = false;
          } else {
            app.chrome.clear_link_drag();
          }
        }
        egui::Event::PointerMoved(pos) => {
          if app.chrome.link_drag_url.is_some()
            && !app.chrome.link_drag_active
            && app.chrome.link_drag_start_pos.is_some()
          {
            let start = app
              .chrome
              .link_drag_start_pos
              .expect("checked is_some above");
            let dx = pos.x - start.0;
            let dy = pos.y - start.1;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq >= LINK_DRAG_THRESHOLD_POINTS * LINK_DRAG_THRESHOLD_POINTS {
              app.chrome.link_drag_active = true;
            }
          }
        }
        egui::Event::PointerButton {
          pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          ..
        } => {
          if app.chrome.link_drag_url.is_some() {
            if app.chrome.link_drag_active && address_bar_rect.is_some_and(|r| r.contains(*pos)) {
              dropped_url = app.chrome.link_drag_url.clone();
            }
            app.chrome.clear_link_drag();
          }
        }
        egui::Event::PointerGone => {
          app.chrome.clear_link_drag();
        }
        _ => {}
      }
    }

    // Ensure we don't keep drag state alive if egui loses track of the pointer/button state.
    if !i.pointer.primary_down() {
      app.chrome.clear_link_drag();
    }

    dropped_url
  });

  if let Some(url) = dropped_url {
    actions.push(ChromeAction::NavigateTo(url));
  }

  actions
}

const HOVER_STATUS_OVERLAY_MAX_WIDTH: f32 = 600.0;
const HOVER_STATUS_OVERLAY_MARGIN: f32 = 10.0;
const HOVER_STATUS_OVERLAY_SLIDE_PX: f32 = 6.0;
const HOVER_STATUS_OVERLAY_PADDING_X: f32 = 10.0;
const HOVER_STATUS_OVERLAY_PADDING_Y: f32 = 6.0;

#[derive(Debug, Default)]
struct HoverStatusOverlayCache {
  cached_url: Option<Arc<str>>,
  text_width_font_id: Option<egui::FontId>,
  text_width_url: Option<Arc<str>>,
  text_width: f32,
  a11y_label_cache: crate::ui::tab_accessible_label::TitlePrefixedLabelCache,
}

impl HoverStatusOverlayCache {
  fn update_cached_url(&mut self, hovered_url_now: Option<&str>) {
    let Some(url_now) = hovered_url_now else {
      return;
    };
    if self.cached_url.as_deref() == Some(url_now) {
      return;
    }
    self.cached_url = Some(Arc::from(url_now));
  }

  fn cached_url(&self) -> Option<Arc<str>> {
    self.cached_url.as_ref().map(Arc::clone)
  }

  fn url_text_width(&mut self, ui: &egui::Ui, url: &Arc<str>, font_id: &egui::FontId) -> f32 {
    if self.text_width_font_id.as_ref() == Some(font_id)
      && self.text_width_url.as_ref().is_some_and(|u| u.as_ref() == url.as_ref())
    {
      return self.text_width;
    }

    self.text_width_font_id = Some(font_id.clone());
    self.text_width_url = Some(Arc::clone(url));
    self.text_width = ui
      .fonts(|f| {
        f.layout_no_wrap(
          url.as_ref().to_owned(),
          font_id.clone(),
          ui.visuals().text_color(),
        )
      })
      .size()
      .x;
    self.text_width
  }

  fn hovered_link_a11y_label(&mut self, url: &Arc<str>) -> Arc<str> {
    self
      .a11y_label_cache
      .get_or_update("Hovered link", url.as_ref())
  }
}

fn hover_status_overlay_anchor_offset(
  screen_rect: egui::Rect,
  content_rect: egui::Rect,
  margin: f32,
) -> egui::Vec2 {
  egui::vec2(
    (content_rect.left() - screen_rect.left()) + margin,
    (content_rect.bottom() - screen_rect.bottom()) - margin,
  )
}

fn hover_status_overlay_max_width(content_rect: egui::Rect, margin: f32) -> f32 {
  (content_rect.width() - margin * 2.0)
    .max(0.0)
    .min(HOVER_STATUS_OVERLAY_MAX_WIDTH)
}

/// Hovered-link status UI: a small bottom overlay "URL bubble" (modern browser style).
///
/// This should be rendered after the page/content area is known so it can anchor to the bottom-left
/// of the viewport without affecting layout.
pub fn hover_status_overlay_ui(
  ctx: &egui::Context,
  app: &BrowserAppState,
  content_rect_points: egui::Rect,
) {
  let hovered_url_now = app
    .active_tab()
    .and_then(|t| t.hovered_url.as_deref())
    .map(str::trim)
    .filter(|s| !s.is_empty());

  let motion = UiMotion::from_ctx(ctx);
  let overlay_id = egui::Id::new("fastr_hover_status_overlay");
  let open_t = motion.animate_bool(
    ctx,
    overlay_id.with("open"),
    hovered_url_now.is_some(),
    motion.durations.hover_fade,
  );
  let open_opacity = open_t.clamp(0.0, 1.0);

  if hovered_url_now.is_none() && open_opacity <= 0.0 {
    return;
  }

  let cache_id = overlay_id.with("cache");
  let mut cache: HoverStatusOverlayCache = ctx.data_mut(|d| {
    std::mem::take(d.get_temp_mut_or_default::<HoverStatusOverlayCache>(cache_id))
  });
  cache.update_cached_url(hovered_url_now);
  let Some(url) = cache.cached_url() else {
    ctx.data_mut(|d| d.insert_temp(cache_id, cache));
    return;
  };
  if url.trim().is_empty() {
    ctx.data_mut(|d| d.insert_temp(cache_id, cache));
    return;
  }

  let margin = HOVER_STATUS_OVERLAY_MARGIN;
  let max_width = hover_status_overlay_max_width(content_rect_points, margin);
  if max_width <= 0.0 {
    ctx.data_mut(|d| d.insert_temp(cache_id, cache));
    return;
  }

  let screen_rect = ctx.screen_rect();
  let mut anchor_offset =
    hover_status_overlay_anchor_offset(screen_rect, content_rect_points, margin);
  if motion.enabled {
    // Micro-interaction: subtle slide from the bottom edge.
    anchor_offset.y += (1.0 - open_opacity) * HOVER_STATUS_OVERLAY_SLIDE_PX;
  }

  egui::Area::new(overlay_id)
    // Keep this under popups/menus (select dropdown, context menus, etc.) while still drawing above
    // the page content.
    .order(egui::Order::Middle)
    .anchor(egui::Align2::LEFT_BOTTOM, anchor_offset)
    // This is purely informational: don't intercept pointer events intended for the page.
    .interactable(false)
    .show(ctx, |ui| {
      ui.visuals_mut().override_text_color =
        Some(with_alpha(ui.visuals().text_color(), open_opacity));

      let high_contrast = chrome_high_contrast_enabled(app);
      let visuals = ui.visuals().clone();

      // Prefer a "bubble" that hugs the URL text instead of spanning the max width.
      let font_id = egui::TextStyle::Small.resolve(ui.style());
      let url_text_width = cache.url_text_width(ui, &url, &font_id);
      let desired_outer_width = url_text_width
        .min((max_width - HOVER_STATUS_OVERLAY_PADDING_X * 2.0).max(0.0))
        + HOVER_STATUS_OVERLAY_PADDING_X * 2.0;
      ui.set_max_width(desired_outer_width.min(max_width));

      let fill = {
        let base = visuals.widgets.inactive.bg_fill;
        let alpha = if high_contrast { 255 } else { 240 };
        with_alpha(
          egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha),
          open_opacity,
        )
      };

      let stroke = {
        let mut stroke = visuals.window_stroke;
        if high_contrast {
          stroke.width = stroke.width.max(2.0);
        }
        stroke.color = with_alpha(stroke.color, open_opacity);
        stroke
      };

      let rounding =
        egui::Rounding::same((visuals.widgets.inactive.rounding.nw * 0.6).clamp(4.0, 8.0));

      let mut frame = egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(rounding)
        .inner_margin(egui::Margin::symmetric(
          HOVER_STATUS_OVERLAY_PADDING_X,
          HOVER_STATUS_OVERLAY_PADDING_Y,
        ));

      if !high_contrast {
        let mut shadow = visuals.popup_shadow;
        shadow.color = with_alpha(shadow.color, open_opacity);
        frame.shadow = shadow;
      }

      frame.show(ui, |ui| {
        let a11y_label = cache.hovered_link_a11y_label(&url);
        let resp = ui.add(
          egui::Label::new(egui::RichText::new(url.as_ref()).small())
            .wrap(false)
            .truncate(true),
        );
        resp.widget_info({
          let label = a11y_label.clone();
          move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
        });
      });
    });

  ctx.data_mut(|d| d.insert_temp(cache_id, cache));
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
  use super::{
    chrome_focus_ring_style, chrome_ui, chrome_ui_with_bookmarks, omnibox_suggestion_a11y_label,
    tab_search_ranked_matches, ChromeAction,
  };
  use crate::ui::a11y_test_util;
  use crate::ui::browser_app::{
    BrowserAppState, BrowserTabState, OpenTabContextMenuState, UiFocusToken,
  };
  use crate::ui::{
    BookmarkStore, OmniboxAction, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
    TabId,
  };

  fn new_context() -> egui::Context {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
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
    raw.focused = true;
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
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn begin_frame_with_accesskit_action_requests(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    requests: Vec<accesskit::ActionRequest>,
  ) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    raw.accesskit_action_requests = requests;
    ctx.begin_frame(raw);
  }

  fn begin_frame_with_time(ctx: &egui::Context, time: f64, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(time);
    raw.focused = true;
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
    raw.focused = true;
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

  fn accesskit_update(output: &egui::FullOutput) -> &accesskit::TreeUpdate {
    output.platform_output.accesskit_update.as_ref().expect(
      "egui did not emit an AccessKit update. Ensure `ctx.enable_accesskit()` was called for the frame under test.",
    )
  }

  fn accesskit_node_by_name<'a>(
    update: &'a accesskit::TreeUpdate,
    name: &str,
  ) -> (accesskit::NodeId, &'a accesskit::Node) {
    let mut matches = update
      .nodes
      .iter()
      .filter(|(_id, node)| node.name().unwrap_or("").trim() == name);
    let Some((id, node)) = matches.next() else {
      let mut seen: Vec<String> = update
        .nodes
        .iter()
        .filter_map(|(id, node)| {
          let node_name = node.name().unwrap_or("").trim();
          if node_name.is_empty() {
            return None;
          }
          Some(format!("{}: {}", id.0.get(), node_name))
        })
        .collect();
      seen.sort();
      panic!("expected AccessKit node named {name:?}, got named nodes={seen:#?}");
    };
    assert!(
      matches.next().is_none(),
      "expected a unique AccessKit node named {name:?}, but multiple nodes matched"
    );
    (*id, node)
  }

  fn accesskit_node_selected(node: &accesskit::Node) -> bool {
    node.is_selected().unwrap_or(false)
  }

  fn apply_close_tab_actions_for_test(
    ctx: &egui::Context,
    app: &mut BrowserAppState,
    actions: Vec<ChromeAction>,
  ) {
    for action in actions {
      let ChromeAction::CloseTab(tab_id) = action else {
        continue;
      };

      if app.tabs.len() <= 1 || app.tab(tab_id).is_none() {
        app.chrome.clear_tab_close(tab_id);
        continue;
      }

      let motion = crate::ui::motion::UiMotion::from_ctx(ctx);
      let animations_enabled = ctx.style().animation_time > 0.0;
      let now = ctx.input(|i| i.time);
      if app
        .chrome
        .request_close_tab(tab_id, now, motion, animations_enabled)
      {
        app.remove_tab(tab_id);
        app.chrome.clear_tab_close(tab_id);
      }
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

  fn hover_status_overlay_texts(output: &egui::FullOutput) -> Vec<String> {
    let mut texts = Vec::new();
    for clipped in &output.shapes {
      collect_text_shapes(&clipped.shape, &mut texts);
    }

    // `new_context` uses an 800x600 screen rect. The hover-status overlay is anchored near the
    // bottom edge; filter by Y position to avoid matching top-chrome labels/buttons.
    texts
      .into_iter()
      .filter(|(_text, pos)| pos.y > 520.0)
      .map(|(text, _pos)| text)
      .collect()
  }

  #[test]
  fn hover_status_overlay_anchor_offset_targets_content_rect_bottom_left() {
    let screen = egui::Rect::from_min_size(egui::Pos2::new(0.0, 0.0), egui::vec2(800.0, 600.0));
    let content = egui::Rect::from_min_max(egui::pos2(120.0, 40.0), egui::pos2(740.0, 500.0));
    let margin = 10.0;

    let offset = super::hover_status_overlay_anchor_offset(screen, content, margin);
    let anchored = screen.left_bottom() + offset;
    assert_eq!(anchored.x, content.left() + margin);
    assert_eq!(anchored.y, content.bottom() - margin);
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

  fn count_drawn_meshes_in_rect(output: &egui::FullOutput, rect: egui::Rect) -> usize {
    fn mesh_bounds(mesh: &egui::epaint::Mesh) -> Option<egui::Rect> {
      let mut min_x = f32::INFINITY;
      let mut min_y = f32::INFINITY;
      let mut max_x = f32::NEG_INFINITY;
      let mut max_y = f32::NEG_INFINITY;

      for v in &mesh.vertices {
        min_x = min_x.min(v.pos.x);
        min_y = min_y.min(v.pos.y);
        max_x = max_x.max(v.pos.x);
        max_y = max_y.max(v.pos.y);
      }

      if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
      }

      Some(egui::Rect::from_min_max(
        egui::pos2(min_x, min_y),
        egui::pos2(max_x, max_y),
      ))
    }

    fn count_in_shape(shape: &egui::epaint::Shape, rect: egui::Rect) -> usize {
      match shape {
        egui::epaint::Shape::Mesh(mesh) => mesh_bounds(mesh)
          .map(|mesh_rect| usize::from(rect.contains(mesh_rect.center())))
          .unwrap_or(0),
        egui::epaint::Shape::Vec(shapes) => shapes.iter().map(|s| count_in_shape(s, rect)).sum(),
        _ => 0,
      }
    }

    output
      .shapes
      .iter()
      .map(|clipped| count_in_shape(&clipped.shape, rect))
      .sum()
  }

  fn right_click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    vec![
      egui::Event::PointerMoved(pos),
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
      },
      egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
      },
    ]
  }

  fn find_text_pos(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Pos2> {
    let mut texts = Vec::new();
    for clipped in shapes {
      collect_text_shapes(&clipped.shape, &mut texts);
    }
    texts
      .into_iter()
      .find_map(|(text, pos)| text.contains(needle).then_some(pos))
  }

  fn collect_text_strings(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
    use std::collections::BTreeSet;

    let mut texts = Vec::new();
    for clipped in shapes {
      collect_text_shapes(&clipped.shape, &mut texts);
    }
    let mut out = BTreeSet::new();
    for (raw, _) in texts {
      let trimmed = raw.trim();
      if !trimmed.is_empty() {
        out.insert(trimmed.to_string());
      }
    }
    out.into_iter().collect()
  }

  fn expect_accesskit_node_named<'a>(
    output: &'a egui::FullOutput,
    name: &str,
  ) -> (accesskit::NodeId, &'a accesskit::Node) {
    let update = output.platform_output.accesskit_update.as_ref().expect(
      "egui did not emit an AccessKit update; ensure ctx.enable_accesskit() was called",
    );
    update
      .nodes
      .iter()
      .find_map(|(id, node)| {
        let node_name = node.name().unwrap_or("").trim();
        (node_name == name).then_some((*id, node))
      })
      .unwrap_or_else(|| {
        let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(output);
        panic!(
          "expected AccessKit node named {name:?} in output.\n\nsnapshot:\n{snapshot}"
        );
      })
  }

  fn begin_frame_with_accesskit_requests(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    accesskit_action_requests: Vec<accesskit::ActionRequest>,
  ) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    raw.accesskit_action_requests = accesskit_action_requests;
    ctx.begin_frame(raw);
  }

  #[test]
  fn focus_ring_strengthens_when_profile_high_contrast_enabled() {
    let ctx = new_context();
    let mut app = BrowserAppState::new();
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let normal = chrome_focus_ring_style(&ctx, &app);
    let _ = ctx.end_frame();

    let ctx = new_context();
    let mut app = BrowserAppState::new();
    app.appearance.high_contrast = true;
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let high = chrome_focus_ring_style(&ctx, &app);
    let _ = ctx.end_frame();

    assert!(
      high.stroke.width >= normal.stroke.width,
      "expected high-contrast focus ring stroke width ({}) to be >= normal ({})",
      high.stroke.width,
      normal.stroke.width
    );
    assert!(
      high.expand >= normal.expand,
      "expected high-contrast focus ring expand ({}) to be >= normal ({})",
      high.expand,
      normal.expand
    );
  }

  #[test]
  fn chrome_emits_accesskit_names_for_core_navigation_controls() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);

    for expected in [
      "Back",
      "Forward",
      "Reload",
      crate::ui::a11y::ADDRESS_BAR_LABEL,
    ] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in chrome output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn downloads_button_accesskit_label_reflects_panel_open_state() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Closed.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    assert!(
      names.iter().any(|n| n == "Show downloads"),
      "expected downloads button to expose \"Show downloads\" when closed.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );

    // Open.
    app.chrome.downloads_panel_open = true;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    assert!(
      names.iter().any(|n| n == "Hide downloads"),
      "expected downloads button to expose \"Hide downloads\" when open.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn hamburger_menu_items_show_hide_labels_reflect_panel_open_state() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: render once so the chrome stores the menu button rect.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let menu_button_rect = expect_temp_rect(&ctx, "chrome_menu_button_rect");

    // Frame 1: click the menu button to open the menu.
    begin_frame(&ctx, left_click_at(menu_button_rect.center()));
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    for expected in ["Show history panel", "Show bookmarks manager"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected hamburger menu label {expected:?}.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }

    // Frame 2: toggle history panel open state; menu item label should switch to "Hide history panel".
    app.chrome.history_panel_open = true;
    app.chrome.bookmarks_manager_open = false;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();
    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    assert!(
      names.iter().any(|n| n == "Hide history panel"),
      "expected hamburger menu history label to switch to \"Hide history panel\".\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );

    // Frame 3: toggle bookmarks manager open state; menu item label should switch accordingly.
    app.chrome.history_panel_open = false;
    app.chrome.bookmarks_manager_open = true;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(&ctx, &mut app, None, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();
    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    assert!(
      names.iter().any(|n| n == "Hide bookmarks manager"),
      "expected hamburger menu bookmarks manager label to switch to \"Hide bookmarks manager\".\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn appearance_popup_emits_accesskit_names_for_accent_controls() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.appearance_popup_open = true;

    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);

    for expected in [
      "Set accent color: Blue",
      "Set accent color: Green",
      "Set accent color: Purple",
      "Set accent color: Orange",
      "Set accent color: Red",
      "Set accent color: Gray",
      "Custom accent color",
      "UI scale",
    ] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in appearance popup output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn appearance_popup_accesskit_roles_for_accent_controls_are_buttons() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.appearance_popup_open = true;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    for expected in [
      "Set accent color: Blue",
      "Set accent color: Green",
      "Set accent color: Purple",
      "Set accent color: Orange",
      "Set accent color: Red",
      "Set accent color: Gray",
      "Custom accent color",
    ] {
      assert!(
        nodes
          .iter()
          .any(|n| n.name == expected && n.role == "Button"),
        "expected {expected:?} to appear as a Button in AccessKit output.\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn chrome_accesskit_roles_for_core_navigation_controls_are_buttons() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    for expected in ["Back", "Forward", "Reload"] {
      assert!(
        nodes
          .iter()
          .any(|n| n.name == expected && n.role == "Button"),
        "expected {expected:?} to appear as a Button in AccessKit output.\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn chrome_accesskit_exposes_address_bar_as_search_box_when_editing() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.request_focus_address_bar = true;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      nodes
        .iter()
        .any(|n| n.name == crate::ui::a11y::ADDRESS_BAR_LABEL && n.role == "SearchBox"),
      "expected address bar to appear as a SearchBox when editing.\n\nsnapshot:\n{snapshot}"
    );
  }

  fn accesskit_node_by_role_and_name<'a>(
    output: &'a egui::FullOutput,
    role: accesskit::Role,
    expected_name: &str,
  ) -> (accesskit::NodeId, &'a accesskit::Node) {
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected AccessKit update to be emitted");
    update
      .nodes
      .iter()
      .find_map(|(id, node)| {
        (node.role() == role && node.name().unwrap_or("").trim() == expected_name)
          .then_some((*id, node))
      })
      .unwrap_or_else(|| {
        let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(output);
        panic!(
          "expected to find AccessKit node with role={role:?} name={expected_name:?}.\n\nsnapshot:\n{snapshot}"
        )
      })
  }

  #[test]
  fn appearance_button_accesskit_expand_collapse_opens_closes_popup_and_manages_focus() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: capture ids + AccessKit node id.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();
    let appearance_button_id = expect_temp_id(&ctx, "chrome_appearance_button_id");
    let (appearance_node_id, _) =
      accesskit_node_by_role_and_name(&output, accesskit::Role::Button, "Appearance");

    // Frame 1: focus the appearance button.
    ctx.memory_mut(|mem| mem.request_focus(appearance_button_id));
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(appearance_button_id)),
      "expected appearance button to have focus"
    );

    // Frame 2: screen-reader Expand should open the popup and focus the first radio button.
    begin_frame_with_accesskit_action_requests(
      &ctx,
      Vec::new(),
      vec![accesskit::ActionRequest {
        action: accesskit::Action::Expand,
        target: appearance_node_id,
        data: None,
      }],
    );
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    assert!(
      app.chrome.appearance_popup_open,
      "expected appearance popup to be open after AccessKit Expand"
    );

    let (_id, node) =
      accesskit_node_by_role_and_name(&output, accesskit::Role::Button, "Appearance");
    assert_eq!(
      node.is_expanded(),
      Some(true),
      "expected appearance button expanded state to be true when popup is open"
    );
    assert!(
      node.supports_action(accesskit::Action::Collapse),
      "expected expanded appearance button to expose Collapse action"
    );
    assert!(
      !node.supports_action(accesskit::Action::Expand),
      "expected expanded appearance button to not expose Expand action"
    );

    let system_radio_id = expect_temp_id(&ctx, "appearance_theme_system_radio_id");
    assert!(
      ctx.memory(|mem| mem.has_focus(system_radio_id)),
      "expected the first appearance theme radio (System) to receive focus when opening the popup"
    );

    // Frame 3: screen-reader Collapse should close the popup and restore focus to the opener.
    begin_frame_with_accesskit_action_requests(
      &ctx,
      Vec::new(),
      vec![accesskit::ActionRequest {
        action: accesskit::Action::Collapse,
        target: appearance_node_id,
        data: None,
      }],
    );
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    assert!(
      !app.chrome.appearance_popup_open,
      "expected appearance popup to be closed after AccessKit Collapse"
    );
    let (_id, node) =
      accesskit_node_by_role_and_name(&output, accesskit::Role::Button, "Appearance");
    assert_eq!(
      node.is_expanded(),
      Some(false),
      "expected appearance button expanded state to be false when popup is closed"
    );
    assert!(
      node.supports_action(accesskit::Action::Expand),
      "expected collapsed appearance button to expose Expand action"
    );
    assert!(
      !node.supports_action(accesskit::Action::Collapse),
      "expected collapsed appearance button to not expose Collapse action"
    );
    assert!(
      ctx.memory(|mem| mem.has_focus(appearance_button_id)),
      "expected focus to return to appearance button after closing the popup"
    );
  }

  #[test]
  fn tab_search_overlay_accesskit_row_labels_have_stable_node_ids_across_filtering() {
    let mut app = BrowserAppState::new();
    let mut tab_a = BrowserTabState::new(TabId(1), "https://alpha.example/a".to_string());
    tab_a.title = Some("Alpha".to_string());
    let mut tab_b = BrowserTabState::new(TabId(2), "https://beta.example/b".to_string());
    tab_b.title = Some("Beta".to_string());
    let mut tab_c = BrowserTabState::new(TabId(3), "https://gamma.example/c".to_string());
    tab_c.title = Some("Gamma".to_string());
    app.push_tab(tab_a, true);
    app.push_tab(tab_b, false);
    app.push_tab(tab_c, false);

    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query = "".to_string();
    app.chrome.tab_search.selected = 0;

    let alpha_label = "Switch to tab: Alpha (alpha.example)";
    let beta_label = "Switch to tab: Beta (beta.example)";
    let gamma_label = "Switch to tab: Gamma (gamma.example)";

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: full tab list.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output0 = ctx.end_frame();
    let names0 = a11y_test_util::accesskit_names_from_full_output(&output0);
    let snapshot0 = a11y_test_util::accesskit_pretty_json_from_full_output(&output0);
    for expected in [alpha_label, beta_label, gamma_label, "Search tabs"] {
      assert!(
        names0.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in tab search output.\n\nnames: {names0:#?}\n\nsnapshot:\n{snapshot0}"
      );
    }
    assert!(
      ctx.memory(|mem| mem.has_focus(super::tab_search_input_id())),
      "expected tab search input to keep keyboard focus while overlay is open"
    );

    let update0 = accesskit_update(&output0);
    let (beta_id_0, _beta_node_0) = accesskit_node_by_name(update0, beta_label);

    // Frame 1: apply a query that filters down to Beta, moving it to row 0. The AccessKit node id
    // should still be stable because the row id is keyed by tab id rather than row index.
    app.chrome.tab_search.query = "beta".to_string();
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output1 = ctx.end_frame();
    let update1 = accesskit_update(&output1);
    let (beta_id_1, _beta_node_1) = accesskit_node_by_name(update1, beta_label);

    assert_eq!(
      beta_id_1, beta_id_0,
      "expected stable AccessKit node ids for tab search rows across filtering.\n\nframe0:\n{}\n\nframe1:\n{}",
      a11y_test_util::accesskit_pretty_json_from_full_output(&output0),
      a11y_test_util::accesskit_pretty_json_from_full_output(&output1),
    );
  }

  #[test]
  fn tab_search_overlay_exposes_selected_row_via_accesskit_selected_state() {
    let mut app = BrowserAppState::new();
    let mut tab_a = BrowserTabState::new(TabId(1), "https://alpha.example/a".to_string());
    tab_a.title = Some("Alpha".to_string());
    let mut tab_b = BrowserTabState::new(TabId(2), "https://beta.example/b".to_string());
    tab_b.title = Some("Beta".to_string());
    let mut tab_c = BrowserTabState::new(TabId(3), "https://gamma.example/c".to_string());
    tab_c.title = Some("Gamma".to_string());
    app.push_tab(tab_a, true);
    app.push_tab(tab_b, false);
    app.push_tab(tab_c, false);

    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query = "".to_string();

    let alpha_label = "Switch to tab: Alpha (alpha.example)";
    let beta_label = "Switch to tab: Beta (beta.example)";
    let gamma_label = "Switch to tab: Gamma (gamma.example)";

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: selection starts at row 0.
    app.chrome.tab_search.selected = 0;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output0 = ctx.end_frame();
    let update0 = accesskit_update(&output0);
    assert!(accesskit_node_selected(
      accesskit_node_by_name(update0, alpha_label).1
    ));
    assert!(!accesskit_node_selected(
      accesskit_node_by_name(update0, beta_label).1
    ));
    assert!(!accesskit_node_selected(
      accesskit_node_by_name(update0, gamma_label).1
    ));

    // Frame 1: move selection to row 1.
    app.chrome.tab_search.selected = 1;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output1 = ctx.end_frame();
    let update1 = accesskit_update(&output1);
    assert!(!accesskit_node_selected(
      accesskit_node_by_name(update1, alpha_label).1
    ));
    assert!(accesskit_node_selected(
      accesskit_node_by_name(update1, beta_label).1
    ));

    // Frame 2: move selection to row 2.
    app.chrome.tab_search.selected = 2;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output2 = ctx.end_frame();
    let update2 = accesskit_update(&output2);
    assert!(!accesskit_node_selected(
      accesskit_node_by_name(update2, beta_label).1
    ));
    assert!(accesskit_node_selected(
      accesskit_node_by_name(update2, gamma_label).1
    ));
  }

  #[test]
  fn downloads_button_accesskit_expanded_state_tracks_panel_open_state() {
    for (open, expected_expanded, supports_expand, supports_collapse) in [
      (false, Some(false), true, false),
      (true, Some(true), false, true),
    ] {
      let mut app = BrowserAppState::new();
      app.push_tab(
        BrowserTabState::new(TabId(1), "about:newtab".to_string()),
        true,
      );
      app.chrome.downloads_panel_open = open;

      let ctx = egui::Context::default();
      ctx.enable_accesskit();

      begin_frame(&ctx, Vec::new());
      let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
      let output = ctx.end_frame();

      let (_id, node) = expect_accesskit_node_named(&output, "Show downloads");
      assert_eq!(
        node.is_expanded(),
        expected_expanded,
        "expected downloads button expanded={expected_expanded:?} when open={open}"
      );
      assert_eq!(
        node.supports_action(accesskit::Action::Expand),
        supports_expand,
        "expected downloads button supports_expand={supports_expand} when open={open}"
      );
      assert_eq!(
        node.supports_action(accesskit::Action::Collapse),
        supports_collapse,
        "expected downloads button supports_collapse={supports_collapse} when open={open}"
      );
    }
  }

  #[test]
  fn downloads_button_accesskit_expand_request_opens_only_when_closed() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.downloads_panel_open = false;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: render once and capture the AccessKit node id for the downloads button.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();
    let (downloads_node_id, _node) = expect_accesskit_node_named(&output, "Show downloads");

    // Frame 1: request Expand. When closed, we should emit a toggle action to open the panel.
    begin_frame_with_accesskit_requests(
      &ctx,
      Vec::new(),
      vec![accesskit::ActionRequest {
        action: accesskit::Action::Expand,
        target: downloads_node_id,
        data: None,
      }],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleDownloadsPanel)),
      "expected ChromeAction::ToggleDownloadsPanel when Expand is requested while closed, got {actions:?}"
    );

    // Frame 2: simulate the panel being open and request Expand again. This should be a no-op
    // (avoid toggling closed due to a mismatched action request).
    app.chrome.downloads_panel_open = true;
    begin_frame_with_accesskit_requests(
      &ctx,
      Vec::new(),
      vec![accesskit::ActionRequest {
        action: accesskit::Action::Expand,
        target: downloads_node_id,
        data: None,
      }],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleDownloadsPanel)),
      "did not expect ChromeAction::ToggleDownloadsPanel when Expand is requested while open, got {actions:?}"
    );
  }

  #[test]
  fn omnibox_accesskit_exposes_suggestions_as_listbox_options() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.request_focus_address_bar = true;
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.suggestions = vec![
      OmniboxSuggestion {
        action: OmniboxAction::Search("cats".to_string()),
        title: Some("cats".to_string()),
        url: None,
        source: OmniboxSuggestionSource::Primary,
      },
      OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl,
        title: Some("Example".to_string()),
        url: Some("https://example.com".to_string()),
        source: OmniboxSuggestionSource::Primary,
      },
    ];
    app.chrome.omnibox.selected = Some(1);

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected AccessKit update from chrome_ui");

    let mut saw_listbox = false;
    let mut options: Vec<(String, bool)> = Vec::new();
    for (_id, node) in &update.nodes {
      let name = node.name().unwrap_or("").trim().to_string();
      if node.role() == accesskit::Role::ListBox && name == "Omnibox suggestions" {
        saw_listbox = true;
      }
      if node.role() == accesskit::Role::ListBoxOption && !name.is_empty() {
        options.push((name, node.is_selected()));
      }
    }

    assert!(
      saw_listbox,
      "expected omnibox suggestion container to be a ListBox named \"Omnibox suggestions\".\n\nsnapshot:\n{snapshot}"
    );

    let expected = app
      .chrome
      .omnibox
      .suggestions
      .iter()
      .enumerate()
      .map(|(idx, s)| (omnibox_suggestion_a11y_label(s), idx == 1))
      .collect::<Vec<_>>();

    for (label, should_be_selected) in expected {
      let found = options.iter().find(|(name, _)| name == &label);
      assert!(
        found.is_some(),
        "expected omnibox option {label:?} in AccessKit output.\n\noptions: {options:#?}\n\nsnapshot:\n{snapshot}"
      );
      let (_name, selected) = found.unwrap();
      assert_eq!(
        *selected, should_be_selected,
        "expected omnibox option {label:?} selected={should_be_selected}, got {selected}.\n\noptions: {options:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn tab_search_accesskit_exposes_results_as_listbox_options() {
    let mut app = BrowserAppState::new();
    let mut tab_a = BrowserTabState::new(TabId(1), "https://a.example".to_string());
    tab_a.title = Some("Alpha".to_string());
    let mut tab_b = BrowserTabState::new(TabId(2), "https://b.example".to_string());
    tab_b.title = Some("Beta".to_string());
    let mut tab_c = BrowserTabState::new(TabId(3), "https://c.example".to_string());
    tab_c.title = Some("Gamma".to_string());
    app.push_tab(tab_a, true);
    app.push_tab(tab_b, false);
    app.push_tab(tab_c, false);

    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query.clear();
    app.chrome.tab_search.selected = 1;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected AccessKit update from chrome_ui");

    let mut saw_listbox = false;
    let mut options: Vec<(String, bool)> = Vec::new();
    for (_id, node) in &update.nodes {
      let name = node.name().unwrap_or("").trim().to_string();
      if node.role() == accesskit::Role::ListBox && name == "Tab search results" {
        saw_listbox = true;
      }
      if node.role() == accesskit::Role::ListBoxOption && !name.is_empty() {
        options.push((name, node.is_selected()));
      }
    }

    assert!(
      saw_listbox,
      "expected tab search results container to be a ListBox named \"Tab search results\".\n\nsnapshot:\n{snapshot}"
    );

    for (name, should_be_selected) in [
      ("Alpha".to_string(), false),
      ("Beta".to_string(), true),
      ("Gamma".to_string(), false),
    ] {
      let found = options.iter().find(|(n, _)| n == &name);
      assert!(
        found.is_some(),
        "expected tab search option {name:?} in AccessKit output.\n\noptions: {options:#?}\n\nsnapshot:\n{snapshot}"
      );
      let (_name, selected) = found.unwrap();
      assert_eq!(
        *selected, should_be_selected,
        "expected tab search option {name:?} selected={should_be_selected}, got {selected}.\n\noptions: {options:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn tab_search_input_accesskit_active_descendant_points_at_selected_row() {
    let mut app = BrowserAppState::new();
    let mut tab_a = BrowserTabState::new(TabId(1), "https://a.example".to_string());
    tab_a.title = Some("Alpha".to_string());
    let mut tab_b = BrowserTabState::new(TabId(2), "https://b.example".to_string());
    tab_b.title = Some("Beta".to_string());
    let mut tab_c = BrowserTabState::new(TabId(3), "https://c.example".to_string());
    tab_c.title = Some("Gamma".to_string());
    app.push_tab(tab_a, true);
    app.push_tab(tab_b, false);
    app.push_tab(tab_c, false);

    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query.clear();
    app.chrome.tab_search.selected = 2;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    let update = accesskit_update(&output);

    let (_input_id, input_node) =
      accesskit_node_by_name(update, crate::ui::a11y::TAB_SEARCH_LABEL);

    let mut selected_row_id: Option<accesskit::NodeId> = None;
    for (id, node) in &update.nodes {
      if node.role() == accesskit::Role::ListBoxOption && accesskit_node_selected(node) {
        assert!(
          selected_row_id.is_none(),
          "expected exactly one selected tab search row in AccessKit output.\n\nsnapshot:\n{snapshot}"
        );
        selected_row_id = Some(*id);
      }
    }
    let selected_row_id = selected_row_id
      .expect("expected a selected tab search row in AccessKit output (selected index 2)");

    assert_eq!(
      input_node.active_descendant(),
      Some(selected_row_id),
      "expected tab search input active-descendant to point at the selected row.\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn omnibox_dropdown_accesskit_node_ids_are_stable_for_existing_suggestions() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    app.chrome.request_focus_address_bar = true;
    app.chrome.omnibox.open = true;

    let target = OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("Example".to_string()),
      url: Some("https://example.com/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    };
    let target_label = omnibox_suggestion_a11y_label(&target);

    app.chrome.omnibox.suggestions = vec![
      target.clone(),
      OmniboxSuggestion {
        action: OmniboxAction::Search("rust".to_string()),
        title: None,
        url: None,
        source: OmniboxSuggestionSource::Primary,
      },
    ];

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: capture the AccessKit node id for the target suggestion.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, true, true, |_| None);
    let output0 = ctx.end_frame();
    let update0 = accesskit_update(&output0);
    let (id0, _node0) = accesskit_node_by_name(update0, &target_label);

    // Frame 1: mutate the suggestion list ordering (new suggestion inserted) but keep the target
    // suggestion text identical. The AccessKit node id should remain stable.
    app.chrome.omnibox.suggestions.insert(
      0,
      OmniboxSuggestion {
        action: OmniboxAction::Search("inserted".to_string()),
        title: None,
        url: None,
        source: OmniboxSuggestionSource::Primary,
      },
    );

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, true, true, |_| None);
    let output1 = ctx.end_frame();
    let update1 = accesskit_update(&output1);
    let (id1, _node1) = accesskit_node_by_name(update1, &target_label);

    assert_eq!(
      id0, id1,
      "expected stable AccessKit node id for omnibox suggestion label {target_label:?} across frames.\n\nframe0:\n{}\n\nframe1:\n{}",
      a11y_test_util::accesskit_pretty_json_from_full_output(&output0),
      a11y_test_util::accesskit_pretty_json_from_full_output(&output1),
    );
  }

  #[test]
  fn omnibox_dropdown_accesskit_selected_state_tracks_selected_index() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    app.chrome.request_focus_address_bar = true;
    app.chrome.omnibox.open = true;

    let a = OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("A".to_string()),
      url: Some("https://a.example/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    };
    let b = OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("B".to_string()),
      url: Some("https://b.example/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    };
    let label_a = omnibox_suggestion_a11y_label(&a);
    let label_b = omnibox_suggestion_a11y_label(&b);

    app.chrome.omnibox.suggestions = vec![a, b];

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 0: select index 0.
    app.chrome.omnibox.selected = Some(0);
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, true, true, |_| None);
    let output0 = ctx.end_frame();
    let update0 = accesskit_update(&output0);
    assert!(accesskit_node_selected(accesskit_node_by_name(update0, &label_a).1));
    assert!(!accesskit_node_selected(accesskit_node_by_name(update0, &label_b).1));

    // Frame 1: move selection to index 1.
    app.chrome.omnibox.selected = Some(1);
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, true, true, |_| None);
    let output1 = ctx.end_frame();
    let update1 = accesskit_update(&output1);
    assert!(!accesskit_node_selected(accesskit_node_by_name(update1, &label_a).1));
    assert!(accesskit_node_selected(accesskit_node_by_name(update1, &label_b).1));
  }

  #[test]
  fn accent_color_swatch_activates_via_keyboard() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );
    app.chrome.appearance_popup_open = true;

    let ctx = egui::Context::default();

    // Frame 0: capture the swatch id via `store_test_id`.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let blue_id = expect_temp_id(&ctx, "appearance_accent_swatch_blue_id");

    // Frame 1: focus the swatch.
    ctx.memory_mut(|mem| mem.request_focus(blue_id));
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(blue_id)),
      "expected focus on the Blue accent swatch"
    );

    // Frame 2: activate via keyboard.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(
      app.appearance.accent_color.as_deref(),
      Some("#3b82f6"),
      "expected keyboard activation to set accent color"
    );
  }

  #[test]
  fn omnibox_suggests_bookmarked_urls() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://example.com/bookmark".to_string(), None, None)
      .unwrap();

    app.chrome.request_focus_address_bar = true;
    let ctx = new_context();
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    app.chrome.address_bar_text = "example".to_string();
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com");
    assert!(!app.chrome.address_bar_has_focus);
    assert!(!app.chrome.address_bar_editing);
  }

  #[test]
  fn try_http_button_emits_navigate_action_for_failed_https() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.error = Some("TLS handshake failed".to_string());
    app.push_tab(tab, true);

    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores the Try HTTP button rect/id.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let try_http_rect = expect_temp_rect(&ctx, "chrome_try_http_button_rect");

    // Frame 1: click the button.
    begin_frame(&ctx, left_click_at(try_http_rect.center()));
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(
        |action| matches!(action, ChromeAction::NavigateTo(url) if url == "http://example.com/")
      ),
      "expected ChromeAction::NavigateTo(\"http://example.com/\"), got {actions:?}"
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    assert_eq!(
      output.repaint_after,
      std::time::Duration::ZERO,
      "expected click-to-focus to request a follow-up repaint so the address bar can enter editing mode"
    );
    assert!(!app.chrome.request_focus_address_bar);
    assert!(!app.chrome.request_select_all_address_bar);

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);
    assert!(app.chrome.address_bar_editing);
  }

  #[test]
  fn chrome_idle_does_not_request_continuous_repaints() {
    use std::time::Duration;

    let ctx = egui::Context::default();
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    // Keep the chrome truly "idle": no panels, popups, or loading indicators.
    app.chrome.appearance_popup_open = false;
    app.chrome.history_panel_open = false;
    app.chrome.bookmarks_manager_open = false;
    app.chrome.tab_search.open = false;
    app.chrome.open_tab_context_menu = None;
    app.chrome.closing_tabs.clear();
    if let Some(tab) = app.active_tab_mut() {
      tab.loading = false;
      tab.load_progress = None;
      tab.hovered_url = None;
      tab.warning = None;
      tab.error = None;
    }

    // Run a few frames with monotonic time and no input. After the first frame (allowing one-time
    // layout/focus settling), the chrome should report "no repaint requested" via a very large
    // `repaint_after` value.
    let mut repaint_after_by_frame = Vec::new();
    for (idx, time) in [0.0_f64, 0.016, 0.032, 0.048, 0.064]
      .into_iter()
      .enumerate()
    {
      begin_frame_with_time(&ctx, time, Vec::new());
      let _actions = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        None,
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let output = ctx.end_frame();
      repaint_after_by_frame.push(output.repaint_after);

      // Allow the first frame (idx=0) to request a follow-up repaint for one-time settling.
      if idx == 0 {
        continue;
      }

      assert!(
        output.repaint_after >= Duration::from_secs(1),
        "idle chrome should not request continuous repaints (expected repaint_after >= 1s after the first frame).\n\
\n\
frame={idx} repaint_after={:?}\n\
\n\
Common causes:\n\
- unconditional `ctx.request_repaint()` / `request_repaint_after(...)`\n\
- an animation left running (loading spinner/progress, tab close animation)\n\
- an overlay/popup kept open (tab search, menus, tooltips)\n\
- a focused TextEdit (cursor blink)\n",
        output.repaint_after
      );
    }

    // Provide the full observed sequence in the assertion output to make regressions easier to
    // diagnose from CI logs.
    assert!(
      repaint_after_by_frame
        .iter()
        .skip(1)
        .all(|d| *d >= Duration::from_secs(1)),
      "idle chrome repaint_after sequence (after first frame) should be >=1s, got: {repaint_after_by_frame:?}"
    );
  }

  #[test]
  fn chrome_requests_continuous_repaint_when_loading_spinner_animates() {
    use std::time::Duration;

    let ctx = egui::Context::default();
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com".to_string());
    tab.loading = true;
    app.push_tab(tab, true);

    // With loading=true and motion enabled (default), the spinner/progress indicator should keep
    // requesting repaints so the animation advances.
    for (idx, time) in [0.0_f64, 0.016, 0.032].into_iter().enumerate() {
      begin_frame_with_time(&ctx, time, Vec::new());
      let _actions = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        None,
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let output = ctx.end_frame();

      // The first frame may do setup work; assert on subsequent frames.
      if idx == 0 {
        continue;
      }

      assert!(
        output.repaint_after <= Duration::from_millis(50),
        "expected loading spinner animation to request frequent repaints (repaint_after <= 50ms).\n\
\n\
frame={idx} repaint_after={:?}\n",
        output.repaint_after
      );
    }
  }

  #[test]
  fn ctrl_l_select_all_is_applied_before_first_typed_character() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );

    // Simulate the user pressing Ctrl/Cmd+L and typing immediately before the next redraw.
    let events = vec![
      egui::Event::Key {
        key: egui::Key::L,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      },
      egui::Event::Text("x".to_string()),
    ];

    let ctx = egui::Context::default();
    begin_frame(&ctx, events);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);
    assert!(app.chrome.address_bar_editing);
    assert!(!app.chrome.request_focus_address_bar);
    assert!(!app.chrome.request_select_all_address_bar);
    assert_eq!(app.chrome.address_bar_text, "x");
  }

  #[test]
  fn ctrl_l_select_all_is_applied_before_paste() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );

    // Simulate the user pressing Ctrl/Cmd+L and pasting immediately before the next redraw.
    // The select-all behavior should be applied before the paste so the paste replaces the URL
    // (not append/insert).
    let events = vec![
      egui::Event::Key {
        key: egui::Key::L,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      },
      egui::Event::Paste("example.com".to_string()),
    ];

    let ctx = egui::Context::default();
    begin_frame(&ctx, events);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);
    assert!(app.chrome.address_bar_editing);
    assert!(!app.chrome.request_focus_address_bar);
    assert!(!app.chrome.request_select_all_address_bar);
    assert_eq!(app.chrome.address_bar_text, "example.com");
    assert!(
      app.chrome.omnibox.open,
      "expected omnibox suggestions to open after pasting into the address bar"
    );
  }

  #[test]
  fn address_bar_copy_copies_selected_text() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );

    let ctx = egui::Context::default();

    // Frame 1: focus + select all.
    app.chrome.request_focus_address_bar = true;
    app.chrome.request_select_all_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let before = app.chrome.address_bar_text.clone();

    // Frame 2: copy via egui platform event.
    begin_frame(&ctx, vec![egui::Event::Copy]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    assert_eq!(output.platform_output.copied_text, "https://example.com/");
    assert_eq!(app.chrome.address_bar_text, before);
  }

  #[test]
  fn address_bar_cut_copies_and_removes_selected_text() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );

    let ctx = egui::Context::default();

    // Frame 1: focus + select all.
    app.chrome.request_focus_address_bar = true;
    app.chrome.request_select_all_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: cut via egui platform event.
    begin_frame(&ctx, vec![egui::Event::Cut]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    assert_eq!(output.platform_output.copied_text, "https://example.com/");
    // Egui `TextEdit` handles `Event::Cut` by removing the current selection. With the entire URL
    // selected, cutting should clear the address bar.
    assert_eq!(app.chrome.address_bar_text, "");
  }

  #[test]
  fn click_type_enter_in_same_frame_emits_navigate_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );

    // Simulate a click-to-focus address bar interaction where winit batches the click and first
    // keystrokes (text + Enter) into the same egui frame.
    let mut events = left_click_at(egui::pos2(400.0, 60.0));
    events.push(egui::Event::Text("example.com".to_string()));
    events.push(key_press(egui::Key::Enter));

    let ctx = egui::Context::default();
    begin_frame(&ctx, events);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NavigateTo(url) if url == "example.com")),
      "expected ChromeAction::NavigateTo(\"example.com\"), got {actions:?}"
    );
  }

  #[test]
  fn click_type_alt_enter_in_same_frame_emits_open_in_new_tab_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );

    // Simulate a click-to-focus address bar interaction where winit batches the click and first
    // keystrokes (text + Alt+Enter) into the same egui frame.
    let mut events = left_click_at(egui::pos2(400.0, 60.0));
    events.push(egui::Event::Text("example.com".to_string()));
    events.push(egui::Event::Key {
      key: egui::Key::Enter,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        alt: true,
        ..Default::default()
      },
    });

    let ctx = egui::Context::default();
    begin_frame(&ctx, events);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(
        action,
        ChromeAction::OpenUrlInNewTab(url) if url == "example.com"
      )),
      "expected ChromeAction::OpenUrlInNewTab(\"example.com\"), got {actions:?}"
    );
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::AddressBarFocusChanged(false))),
      "expected ChromeAction::AddressBarFocusChanged(false), got {actions:?}"
    );
    assert!(
      !app.chrome.address_bar_has_focus,
      "expected address bar to be blurred after Alt+Enter"
    );
    assert!(
      !app.chrome.address_bar_editing,
      "expected address bar editing to end after Alt+Enter"
    );
  }

  #[test]
  fn hover_status_overlay_shows_hovered_url() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.active_tab_mut().unwrap().hovered_url = Some("https://example.com/".to_string());

    let ctx = new_context();
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    super::hover_status_overlay_ui(&ctx, &app, ctx.screen_rect());
    let output = ctx.end_frame();

    let texts = hover_status_overlay_texts(&output);
    assert!(
      texts.iter().any(|t| t.contains("https://example.com/")),
      "expected hovered URL in hover-status overlay texts, got {texts:?}"
    );
  }

  #[test]
  fn chrome_shows_zoom_percent_when_non_default() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.active_tab_mut().unwrap().zoom = crate::ui::zoom::zoom_in(crate::ui::zoom::DEFAULT_ZOOM);
    let expected = format!(
      "{}%",
      crate::ui::zoom::zoom_percent(app.active_tab().unwrap().zoom)
    );

    let ctx = new_context();
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    let texts = collect_text_strings(&output.shapes);
    assert!(
      texts.iter().any(|t| t.contains(&expected)),
      "expected zoom percent {expected:?} in chrome texts, got {texts:?}"
    );
  }

  #[test]
  fn tab_search_ranks_title_prefix_above_infix() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut a = BrowserTabState::new(tab_a, "https://a.example/".to_string());
    a.title = Some("GitHub".to_string());
    let mut b = BrowserTabState::new(tab_b, "https://b.example/".to_string());
    b.title = Some("My git repo".to_string());

    let matches = tab_search_ranked_matches("git", &[a, b]);
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].tab_id, tab_a);
    assert_eq!(matches[1].tab_id, tab_b);
  }

  #[test]
  fn tab_search_matches_url_when_title_does_not_match() {
    let tab_a = TabId(1);
    let mut a = BrowserTabState::new(tab_a, "https://example.com/path".to_string());
    a.title = Some("Unrelated".to_string());

    let matches = tab_search_ranked_matches("example.com", &[a]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].tab_id, tab_a);
  }

  #[test]
  fn tab_search_empty_query_returns_all_tabs_in_order() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let a = BrowserTabState::new(tab_a, "https://a.example/".to_string());
    let b = BrowserTabState::new(tab_b, "https://b.example/".to_string());

    let matches = tab_search_ranked_matches("", &[a, b]);
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].tab_id, tab_a);
    assert_eq!(matches[1].tab_id, tab_b);
  }

  #[test]
  fn ctrl_shift_a_opens_tab_search_overlay() {
    let mut app = BrowserAppState::new();

    let ctx = new_context_with_key(
      egui::Key::A,
      egui::Modifiers {
        command: true,
        shift: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenTabSearch)),
      "expected ChromeAction::OpenTabSearch, got {actions:?}"
    );
    assert!(app.chrome.tab_search.open, "expected tab search to be open");
  }

  #[test]
  fn enter_activates_selected_tab_from_tab_search_overlay() {
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

    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query.clear();
    app.chrome.tab_search.selected = 1;

    let ctx = new_context_with_key(egui::Key::Enter, Default::default());
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_b)),
      "expected Enter to activate tab {tab_b:?}, got {actions:?}"
    );
    assert!(
      !app.chrome.tab_search.open,
      "expected tab search to be closed after activation"
    );
  }

  #[test]
  fn escape_closes_tab_search_overlay() {
    let mut app = BrowserAppState::new();
    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query = "anything".to_string();
    app.chrome.tab_search.selected = 0;

    let ctx = new_context_with_key(egui::Key::Escape, Default::default());
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTabSearch)),
      "expected ChromeAction::CloseTabSearch, got {actions:?}"
    );
    assert!(
      !app.chrome.tab_search.open,
      "expected tab search to be closed"
    );
  }

  #[test]
  fn escape_closing_tab_search_restores_focus_to_address_bar_when_opened_from_it() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 1: focus the address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, vec![]);
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      app.chrome.address_bar_has_focus,
      "expected address bar to be focused"
    );

    // Frame 2: open tab search via Ctrl/Cmd+Shift+A. The overlay input should take focus.
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::A,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          command: true,
          shift: true,
          ..Default::default()
        },
      }],
    );
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.tab_search.open, "expected tab search to be open");
    assert!(
      ctx.memory(|mem| mem.has_focus(super::tab_search_input_id())),
      "expected tab search input to have focus"
    );

    // Frame 3: press Escape to close the overlay. Focus should return to the address bar (or at
    // least egui should no longer think a hidden text edit has focus).
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      !app.chrome.tab_search.open,
      "expected tab search to be closed"
    );
    assert!(
      app.chrome.address_bar_has_focus || !ctx.wants_keyboard_input(),
      "expected focus to return to address bar or egui to stop wanting keyboard input"
    );
  }

  #[test]
  fn click_outside_closes_tab_search_overlay() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.chrome.tab_search.open = true;
    app.chrome.tab_search.query.clear();
    app.chrome.tab_search.selected = 0;

    let ctx = egui::Context::default();
    begin_frame(&ctx, left_click_at(egui::pos2(12.0, 590.0)));
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::CloseTabSearch)),
      "expected ChromeAction::CloseTabSearch, got {actions:?}"
    );
    assert!(
      !app.chrome.tab_search.open,
      "expected tab search to be closed"
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::FocusAddressBar)),
      "expected ChromeAction::FocusAddressBar, got {actions:?}"
    );
  }

  #[test]
  fn close_tab_shortcut_animates_then_closes() {
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
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };

    // Frame 1: Ctrl/Cmd+W should request a close, but the tab should remain while the animation
    // runs (motion enabled).
    begin_frame_with_time(
      &ctx,
      0.0,
      vec![egui::Event::Key {
        key: egui::Key::W,
        pressed: true,
        repeat: false,
        modifiers,
      }],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    apply_close_tab_actions_for_test(&ctx, &mut app, actions);
    let _ = ctx.end_frame();

    assert!(
      app.tab(tab_a).is_some(),
      "tab should still exist immediately after a close request when motion is enabled"
    );
    assert!(
      app.chrome.closing_tabs.contains_key(&tab_a),
      "expected tab to be marked as closing"
    );

    // Frame 2: advance past the close duration; the tab strip should emit another CloseTab request
    // and the tab should be closed.
    let duration = crate::ui::motion::UiMotion::from_ctx(&ctx)
      .durations
      .tab_close as f64;
    begin_frame_with_time(&ctx, duration + 0.05, Vec::new());
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    apply_close_tab_actions_for_test(&ctx, &mut app, actions);
    let _ = ctx.end_frame();

    assert!(
      app.tab(tab_a).is_none(),
      "expected tab to be closed after animation completes"
    );
    assert_eq!(app.tabs.len(), 1);
    assert!(
      app.tab(tab_b).is_some(),
      "expected remaining tab to still exist"
    );
  }

  #[test]
  fn close_tab_shortcut_is_immediate_when_reduced_motion() {
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
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    begin_frame_with_time(
      &ctx,
      0.0,
      vec![egui::Event::Key {
        key: egui::Key::W,
        pressed: true,
        repeat: false,
        modifiers,
      }],
    );
    crate::ui::motion::UiMotion::set_ctx_reduced_motion(&ctx, true);
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    apply_close_tab_actions_for_test(&ctx, &mut app, actions);
    let _ = ctx.end_frame();

    assert!(
      app.tab(tab_a).is_none(),
      "expected reduced-motion close to remove the tab immediately"
    );
    assert_eq!(app.tabs.len(), 1);
    assert!(app.tab(tab_b).is_some());
  }

  #[test]
  fn ctrl_k_emits_focus_address_bar_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::K, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenFindInPage)),
      "expected ChromeAction::OpenFindInPage, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_s_emits_save_page_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::S, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::SavePage)),
      "expected ChromeAction::SavePage, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_p_emits_print_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::P, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::PrintPage)),
      "expected ChromeAction::PrintPage, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_l_focuses_address_bar_even_when_find_bar_has_focus() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };

    // Frame 1: open find bar (Ctrl/Cmd+F), which focuses the find input.
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::F,
        pressed: true,
        repeat: false,
        modifiers,
      }],
    );
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(app.active_tab().is_some_and(|tab| tab.find.open));

    // Frame 2: Ctrl/Cmd+L should still focus the address bar (even though egui wants keyboard input).
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::L,
        pressed: true,
        repeat: false,
        modifiers,
      }],
    );
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::FocusAddressBar)),
      "expected ChromeAction::FocusAddressBar, got {actions:?}"
    );
    assert!(app.chrome.address_bar_has_focus);
  }

  #[test]
  fn ctrl_f_opens_find_bar_and_immediate_typing_emits_find_query() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let events = vec![
      egui::Event::Key {
        key: egui::Key::F,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      },
      egui::Event::Text("x".to_string()),
    ];

    let ctx = egui::Context::default();
    begin_frame(&ctx, events);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      app.active_tab().is_some_and(|tab| tab.find.open),
      "expected find bar to be open"
    );
    assert_eq!(
      app.active_tab().map(|tab| tab.find.query.as_str()),
      Some("x")
    );
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenFindInPage)),
      "expected ChromeAction::OpenFindInPage, got {actions:?}"
    );
    assert!(
      actions.iter().any(|action| matches!(
        action,
        ChromeAction::FindQuery { tab_id: id, query, case_sensitive } if *id == tab_id && query == "x" && !*case_sensitive
      )),
      "expected ChromeAction::FindQuery for {tab_id:?}, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_f_closes_omnibox_dropdown() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Type input to open omnibox dropdown.
    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(app.chrome.omnibox.open);
    assert!(!app.chrome.omnibox.suggestions.is_empty());

    // Ctrl/Cmd+F opens find bar and should close the omnibox dropdown so it doesn't keep focus.
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::F,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          command: true,
          ..Default::default()
        },
      }],
    );
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      app.active_tab().is_some_and(|tab| tab.find.open),
      "expected find bar to be open"
    );
    assert!(
      !app.chrome.omnibox.open,
      "expected omnibox dropdown to be closed"
    );
  }

  #[test]
  fn ctrl_d_emits_toggle_bookmark_for_active_tab_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::D, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleBookmarkForActiveTab)),
      "expected ChromeAction::ToggleBookmarkForActiveTab, got {actions:?}"
    );
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn ctrl_h_emits_toggle_history_panel_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::H, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleHistoryPanel)),
      "expected ChromeAction::ToggleHistoryPanel, got {actions:?}"
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn cmd_y_emits_toggle_history_panel_action() {
    let mut app = BrowserAppState::new();
    let modifiers = egui::Modifiers {
      command: true,
      ..Default::default()
    };
    let ctx = new_context_with_key(egui::Key::Y, modifiers);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleHistoryPanel)),
      "expected ChromeAction::ToggleHistoryPanel, got {actions:?}"
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::Reload)),
      "expected ChromeAction::Reload, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_n_emits_new_window_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::N,
      egui::Modifiers {
        command: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NewWindow)),
      "expected ChromeAction::NewWindow, got {actions:?}"
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
  fn ctrl_shift_b_emits_toggle_bookmarks_bar_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::B,
      egui::Modifiers {
        command: true,
        shift: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleBookmarksBar)),
      "expected ChromeAction::ToggleBookmarksBar, got {actions:?}"
    );
  }

  #[test]
  fn ctrl_shift_delete_emits_open_clear_browsing_data_dialog_even_when_address_bar_focused() {
    let mut app = BrowserAppState::new();
    app.chrome.address_bar_has_focus = true;
    app.chrome.address_bar_editing = true;

    let ctx = new_context_with_key(
      egui::Key::Delete,
      egui::Modifiers {
        command: true,
        shift: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenClearBrowsingDataDialog)),
      "expected ChromeAction::OpenClearBrowsingDataDialog, got {actions:?}"
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::CloseTab(id) if id == tab_a)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::CloseTab(id) if id == tab_a)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!((app.active_tab().unwrap().zoom - crate::ui::zoom::DEFAULT_ZOOM).abs() < f32::EPSILON);
  }

  #[test]
  fn ctrl_shift_o_emits_toggle_bookmarks_manager_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );

    let ctx = new_context_with_key(
      egui::Key::O,
      egui::Modifiers {
        command: true,
        shift: true,
        ..Default::default()
      },
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleBookmarksManager)),
      "expected ChromeAction::ToggleBookmarksManager, got {actions:?}"
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_b)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_b)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_a)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_a)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_a)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_b)),
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: let egui apply the focus request.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::CloseTab(id) if id == tab_a)),
      "expected middle-click to close tab {tab_a:?}, got {actions:?}"
    );
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_a)),
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
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: measure the first tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected first tab rect to be recorded");
    let _ = ctx.end_frame();

    begin_frame(&ctx, middle_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _ = chrome_ui_with_bookmarks(&ctx_wide, &mut app, None, true, |_| None);
    let (wide_strip, wide_tabs) =
      super::tab_strip::load_test_layout(&ctx_wide).expect("missing tab strip layout metrics");
    let _ = ctx_wide.end_frame();

    // Narrow frame.
    let ctx_narrow = egui::Context::default();
    begin_frame_with_screen_size(&ctx_narrow, egui::vec2(240.0, 600.0), Vec::new());
    let _ = chrome_ui_with_bookmarks(&ctx_narrow, &mut app, None, true, |_| None);
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
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: grab the tab strip rect so we can click the "+" button (pinned to the right edge).
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let (strip_rect, _tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    // Frame 3: click the "+" button and ensure we get the expected action.
    let click_pos = egui::pos2(strip_rect.max.x - 10.0, strip_rect.center().y);
    begin_frame(&ctx, left_click_at(click_pos));
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::NewTab)),
      "expected ChromeAction::NewTab, got {actions:?}"
    );
  }

  #[test]
  fn pinned_tab_can_be_activated_by_click() {
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
    assert!(app.pin_tab(tab_a));

    let ctx = egui::Context::default();

    // Frame 1: warm up layout.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Frame 2: measure the pinned tab rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let tab_rect = tab_rects
      .first()
      .copied()
      .expect("expected pinned tab rect to be recorded");
    let _ = ctx.end_frame();

    // Frame 3: click the pinned tab.
    begin_frame(&ctx, left_click_at(tab_rect.center()));
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_a)),
      "expected click to activate pinned tab {tab_a:?}, got {actions:?}"
    );
  }

  #[test]
  fn pinned_tabs_do_not_render_close_button() {
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
    assert!(app.pin_tab(tab_a));

    let ctx = egui::Context::default();
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let output = ctx.end_frame();

    let pinned_rect = tab_rects
      .iter()
      .find(|r| r.width() < 100.0)
      .copied()
      .expect("expected a pinned tab rect");
    let unpinned_rect = tab_rects
      .iter()
      .find(|r| r.width() > 100.0)
      .copied()
      .expect("expected an unpinned tab rect");

    // The pinned tab does not render an explicit close button; the unpinned tab does.
    assert_eq!(count_drawn_meshes_in_rect(&output, pinned_rect), 0);
    assert_eq!(count_drawn_meshes_in_rect(&output, unpinned_rect), 1);
  }

  #[test]
  fn dragging_tab_label_reorders_tabs() {
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

    // Frame 0: read the tab strip layout so we can target a specific tab rect (more robust than
    // hard-coded coordinates).
    begin_frame_with_screen_size(&ctx, egui::vec2(800.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let (_strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    let rev_before = app.session_revision();

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
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: drag to the right (past the second tab's center).
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![egui::Event::PointerMoved(drag_pos)],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::DetachTab(_))),
      "expected reorder drag not to detach, got {actions:?}"
    );
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
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::DetachTab(_))),
      "expected drop inside strip not to detach, got {actions:?}"
    );
    let _ = ctx.end_frame();

    assert_eq!(
      app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
      vec![tab_b, tab_a]
    );
    assert!(
      app.session_revision() > rev_before,
      "expected drag reorder to bump session revision"
    );
    assert_eq!(app.active_tab_id(), Some(tab_a));
    assert!(app.chrome.dragging_tab_id.is_none());
  }

  #[test]
  fn dragging_hovered_link_below_threshold_does_not_navigate() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .active_tab_mut()
      .expect("expected active tab")
      .hovered_url = Some("https://example.com".to_string());

    let ctx = egui::Context::default();

    // Frame 0: layout the chrome and capture the address bar rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let address_bar_rect = expect_temp_rect(&ctx, "chrome_address_bar_rect");

    // Press just outside (below) the address bar, then move only slightly into the address bar and
    // release. The total travel stays below the drag threshold and should not be treated as a link
    // drag/drop (avoids false positives when the worker hover URL is stale).
    let press_pos = egui::pos2(address_bar_rect.center().x, address_bar_rect.max.y + 1.0);
    let drop_pos = egui::pos2(address_bar_rect.center().x, address_bar_rect.max.y - 1.0);

    // Frame 1: press outside the address bar.
    begin_frame(
      &ctx,
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
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: move less than the drag threshold.
    begin_frame(&ctx, vec![egui::Event::PointerMoved(drop_pos)]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: release over the address bar.
    begin_frame(
      &ctx,
      vec![
        egui::Event::PointerMoved(drop_pos),
        egui::Event::PointerButton {
          pos: drop_pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: egui::Modifiers::default(),
        },
      ],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions
        .iter()
        .any(|a| matches!(a, ChromeAction::NavigateTo(_))),
      "expected link drag below threshold not to navigate, got {actions:?}"
    );
  }

  #[test]
  fn pressing_over_address_bar_does_not_start_link_drag_even_if_hovered_url_is_set() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .active_tab_mut()
      .expect("expected active tab")
      .hovered_url = Some("https://example.com".to_string());

    let ctx = egui::Context::default();

    // Frame 0: layout the chrome and capture the address bar rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let address_bar_rect = expect_temp_rect(&ctx, "chrome_address_bar_rect");

    let press_pos = address_bar_rect.center();
    let drag_pos = egui::pos2(
      (press_pos.x + super::LINK_DRAG_THRESHOLD_POINTS + 20.0).min(address_bar_rect.max.x - 1.0),
      press_pos.y,
    );

    // Frame 1: press inside the address bar.
    begin_frame(
      &ctx,
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
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: move beyond the drag threshold (still shouldn't start a link drag).
    begin_frame(&ctx, vec![egui::Event::PointerMoved(drag_pos)]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: release inside the address bar.
    begin_frame(
      &ctx,
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
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      !actions
        .iter()
        .any(|a| matches!(a, ChromeAction::NavigateTo(_))),
      "expected press over address bar not to start link drag navigation, got {actions:?}"
    );
  }

  #[test]
  fn dragging_hovered_link_to_address_bar_emits_navigate_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app
      .active_tab_mut()
      .expect("expected active tab")
      .hovered_url = Some("https://example.com".to_string());

    let ctx = egui::Context::default();

    // Frame 0: layout the chrome and capture the address bar rect.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let address_bar_rect = expect_temp_rect(&ctx, "chrome_address_bar_rect");

    let press_pos = egui::pos2(address_bar_rect.center().x, address_bar_rect.max.y + 120.0);
    let drag_pos = egui::pos2(
      press_pos.x,
      press_pos.y + super::LINK_DRAG_THRESHOLD_POINTS + 10.0,
    );
    let drop_pos = address_bar_rect.center();

    // Frame 1: press on a hovered link (outside the address bar).
    begin_frame(
      &ctx,
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
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: move far enough to exceed the drag threshold.
    begin_frame(&ctx, vec![egui::Event::PointerMoved(drag_pos)]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: release over the address bar.
    begin_frame(
      &ctx,
      vec![
        egui::Event::PointerMoved(drop_pos),
        egui::Event::PointerButton {
          pos: drop_pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: egui::Modifiers::default(),
        },
      ],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|a| matches!(a, ChromeAction::NavigateTo(url) if url == "https://example.com")),
      "expected ChromeAction::NavigateTo(\"https://example.com\"), got {actions:?}"
    );
  }

  #[test]
  fn dragging_tab_outside_strip_emits_detach_tab_action() {
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

    // Frame 0: read the tab strip layout so we can target a specific tab rect.
    begin_frame_with_screen_size(&ctx, egui::vec2(800.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let (strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    let press_pos = tab_rects.first().expect("expected first tab rect").center();
    let drag_start_pos = egui::pos2(press_pos.x + 80.0, press_pos.y);
    let drag_pos = egui::pos2(strip_rect.center().x, strip_rect.top() - 200.0);

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
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: start a drag inside the strip so egui enters drag mode.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![egui::Event::PointerMoved(drag_start_pos)],
    );
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: drag outside the tab strip.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![egui::Event::PointerMoved(drag_pos)],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::DetachTab(id) if id == tab_a)),
      "expected drag-out to emit DetachTab({tab_a:?}), got {actions:?}"
    );
    assert!(app.chrome.dragging_tab_id.is_none());
  }

  #[test]
  fn releasing_tab_outside_strip_emits_detach_tab_action() {
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

    // Frame 0: read the tab strip layout so we can target a specific tab rect.
    begin_frame_with_screen_size(&ctx, egui::vec2(800.0, 600.0), Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let (strip_rect, tab_rects) =
      super::tab_strip::load_test_layout(&ctx).expect("missing tab strip layout metrics");
    let _ = ctx.end_frame();

    let press_pos = tab_rects.first().expect("expected first tab rect").center();
    let drag_start_pos = egui::pos2(press_pos.x + 80.0, press_pos.y);

    // Release just outside the strip bounds, but still within the detach drag threshold region.
    // This ensures we cover the release-driven detach path (vs. detach-on-drag).
    let release_pos = egui::pos2(strip_rect.center().x, strip_rect.top() - 10.0);
    assert!(
      strip_rect
        .expand(super::tab_strip::TAB_DETACH_DRAG_THRESHOLD)
        .contains(release_pos),
      "expected release_pos to be inside the detach threshold expansion"
    );
    assert!(
      !strip_rect.contains(release_pos),
      "expected release_pos to be outside strip_rect to trigger release detach"
    );

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
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: start a drag inside the strip so egui enters drag mode.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![egui::Event::PointerMoved(drag_start_pos)],
    );
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: release just outside the strip.
    begin_frame_with_screen_size(
      &ctx,
      egui::vec2(800.0, 600.0),
      vec![
        egui::Event::PointerMoved(release_pos),
        egui::Event::PointerButton {
          pos: release_pos,
          button: egui::PointerButton::Primary,
          pressed: false,
          modifiers: egui::Modifiers::default(),
        },
      ],
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::DetachTab(id) if id == tab_a)),
      "expected release-outside-strip to emit DetachTab({tab_a:?}), got {actions:?}"
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );

    app.chrome.address_bar_text.clear();
    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Type input that produces a primary (search) suggestion.
    begin_frame(&ctx, vec![egui::Event::Text("cats".into())]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
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
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Type input that matches about pages provider.
    begin_frame(&ctx, vec![egui::Event::Text("about".into())]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
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
  fn omnibox_inline_autocomplete_selects_suffix_and_accepts_with_right_arrow() {
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
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    let address_bar_id = expect_temp_id(&ctx, "chrome_address_bar_text_edit_id");

    // Type a prefix that can be completed by the about-pages provider.
    begin_frame(&ctx, vec![egui::Event::Text("about:n".into())]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "about:newtab");
    let state =
      egui::text_edit::TextEditState::load(&ctx, address_bar_id).expect("expected TextEdit state");
    let range = state
      .ccursor_range()
      .expect("expected address bar to have a cursor range");
    assert_eq!(range.primary.index, "about:n".chars().count());
    assert_eq!(range.secondary.index, "about:newtab".chars().count());

    // Typing should replace the selected suffix.
    begin_frame(&ctx, vec![egui::Event::Text("x".into())]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert_eq!(app.chrome.address_bar_text, "about:nx");

    // Backspace should restore the prefix and re-trigger inline completion.
    begin_frame(&ctx, vec![key_press(egui::Key::Backspace)]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert_eq!(app.chrome.address_bar_text, "about:newtab");

    // Right arrow accepts completion by collapsing selection to the end.
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowRight)]);
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert_eq!(app.chrome.address_bar_text, "about:newtab");
    let state =
      egui::text_edit::TextEditState::load(&ctx, address_bar_id).expect("expected TextEdit state");
    let range = state
      .ccursor_range()
      .expect("expected address bar to have a cursor range");
    let end = "about:newtab".chars().count();
    assert_eq!(range.primary.index, end);
    assert_eq!(range.secondary.index, end);
  }

  #[test]
  fn omnibox_typing_opens_and_arrow_down_previews_first_suggestion() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );

    // Ensure typed input doesn't append to the active tab URL.
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);

    // Type input.
    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.omnibox.open);
    assert!(!app.chrome.omnibox.suggestions.is_empty());

    // ArrowDown previews first suggestion.
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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

    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://example.com/".to_string(), None, None)
      .unwrap();

    let ctx = egui::Context::default();

    // Focus address bar.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.address_bar_has_focus);

    // Type input that matches the bookmark.
    begin_frame(&ctx, vec![egui::Event::Text("exam".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(app.chrome.omnibox.open, "expected omnibox dropdown to open");
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
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "https://example.com/");

    // Escape should close dropdown and restore original typed input without blurring.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // First Escape closes dropdown and keeps focus.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(app.chrome.address_bar_has_focus);

    // Second Escape should blur and revert to active tab URL.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
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
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(
        |action| matches!(action, ChromeAction::NavigateTo(url) if url == "https://example.com/")
      ),
      "expected ChromeAction::NavigateTo(\"https://example.com/\"), got {actions:?}"
    );
  }

  #[test]
  fn omnibox_alt_enter_with_selection_emits_open_in_new_tab_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example".to_string()),
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![egui::Event::Text("example.com".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::Enter,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          alt: true,
          ..Default::default()
        },
      }],
    );
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(
        action,
        ChromeAction::OpenUrlInNewTab(url) if url == "https://example.com/"
      )),
      "expected ChromeAction::OpenUrlInNewTab(\"https://example.com/\"), got {actions:?}"
    );
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::AddressBarFocusChanged(false))),
      "expected ChromeAction::AddressBarFocusChanged(false), got {actions:?}"
    );
    assert!(
      !app.chrome.address_bar_has_focus,
      "expected address bar to be blurred after Alt+Enter"
    );
    assert!(
      !app.chrome.address_bar_editing,
      "expected address bar editing to end after Alt+Enter"
    );
  }

  #[test]
  fn omnibox_alt_enter_with_switch_to_tab_selection_emits_activate_tab_action() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "https://example.com/".to_string()),
      false,
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Type something that matches tab_b's URL so OpenTabsProvider yields an ActivateTab suggestion.
    begin_frame(&ctx, vec![egui::Event::Text("example".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // ArrowDown selects the primary suggestion first; ArrowDown again should select the open-tab
    // ("switch to tab") suggestion.
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::Enter,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          alt: true,
          ..Default::default()
        },
      }],
    );
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, &ChromeAction::ActivateTab(id) if id == tab_b)),
      "expected ChromeAction::ActivateTab({tab_b:?}), got {actions:?}"
    );
    assert!(
      !actions
        .iter()
        .any(|action| matches!(action, ChromeAction::OpenUrlInNewTab(_))),
      "did not expect ChromeAction::OpenUrlInNewTab, got {actions:?}"
    );
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::AddressBarFocusChanged(false))),
      "expected ChromeAction::AddressBarFocusChanged(false), got {actions:?}"
    );
    assert!(
      !app.chrome.address_bar_has_focus,
      "expected address bar to be blurred after Alt+Enter"
    );
    assert!(
      !app.chrome.address_bar_editing,
      "expected address bar editing to end after Alt+Enter"
    );
  }

  #[test]
  fn omnibox_alt_enter_with_search_selection_emits_open_in_new_tab_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "about:newtab".to_string()),
      true,
    );
    app.chrome.address_bar_text.clear();

    let ctx = egui::Context::default();

    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Type a search query (not URL-like) so the primary omnibox suggestion is a Search action.
    begin_frame(&ctx, vec![egui::Event::Text("cats".into())]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    // Select the primary suggestion.
    begin_frame(&ctx, vec![key_press(egui::Key::ArrowDown)]);
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::Enter,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
          alt: true,
          ..Default::default()
        },
      }],
    );
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      actions.iter().any(|action| matches!(
        action,
        ChromeAction::OpenUrlInNewTab(url) if url == "https://duckduckgo.com/?q=cats"
      )),
      "expected ChromeAction::OpenUrlInNewTab(\"https://duckduckgo.com/?q=cats\"), got {actions:?}"
    );
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::AddressBarFocusChanged(false))),
      "expected ChromeAction::AddressBarFocusChanged(false), got {actions:?}"
    );
    assert!(
      !app.chrome.address_bar_has_focus,
      "expected address bar to be blurred after Alt+Enter"
    );
    assert!(
      !app.chrome.address_bar_editing,
      "expected address bar editing to end after Alt+Enter"
    );
  }

  fn expect_temp_rect(ctx: &egui::Context, key: &'static str) -> egui::Rect {
    ctx
      .data(|d| d.get_temp::<egui::Rect>(egui::Id::new(key)))
      .unwrap_or_else(|| panic!("expected temp rect {key:?}"))
  }

  fn expect_temp_id(ctx: &egui::Context, key: &'static str) -> egui::Id {
    ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new(key)))
      .unwrap_or_else(|| panic!("expected temp id {key:?}"))
  }

  fn nav_row_tab_order(ctx: &egui::Context) -> Vec<egui::Id> {
    vec![
      expect_temp_id(ctx, "chrome_back_button_id"),
      expect_temp_id(ctx, "chrome_forward_button_id"),
      expect_temp_id(ctx, "chrome_reload_stop_button_id"),
      expect_temp_id(ctx, "chrome_home_button_id"),
      // Zoom controls are present in non-compact layout (800px wide test context).
      expect_temp_id(ctx, "chrome_zoom_out_button_id"),
      expect_temp_id(ctx, "chrome_zoom_reset_button_id"),
      expect_temp_id(ctx, "chrome_zoom_in_button_id"),
      // Address bar + right-side actions (left-to-right): address bar, downloads, bookmark, menu, appearance.
      expect_temp_id(ctx, "chrome_address_bar_text_edit_id"),
      expect_temp_id(ctx, "chrome_downloads_button_id"),
      expect_temp_id(ctx, "chrome_bookmark_star_id"),
      expect_temp_id(ctx, "chrome_menu_button_id"),
      expect_temp_id(ctx, "chrome_appearance_button_id"),
    ]
  }

  fn nav_row_tab_order_compact(ctx: &egui::Context) -> Vec<egui::Id> {
    vec![
      expect_temp_id(ctx, "chrome_back_button_id"),
      expect_temp_id(ctx, "chrome_forward_button_id"),
      expect_temp_id(ctx, "chrome_reload_stop_button_id"),
      expect_temp_id(ctx, "chrome_home_button_id"),
      // Compact mode omits zoom out/in controls (and only conditionally shows a zoom reset pill).
      expect_temp_id(ctx, "chrome_address_bar_text_edit_id"),
      expect_temp_id(ctx, "chrome_downloads_button_id"),
      expect_temp_id(ctx, "chrome_bookmark_star_id"),
      expect_temp_id(ctx, "chrome_menu_button_id"),
      expect_temp_id(ctx, "chrome_appearance_button_id"),
    ]
  }

  #[test]
  fn chrome_key_controls_have_stable_ids_across_identical_renders() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "about:newtab".to_string()),
      true,
    );

    let ctx = egui::Context::default();

    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let first = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );

    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let second = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );

    assert_eq!(
      first, second,
      "expected ids for key chrome controls to remain stable across identical renders"
    );
  }

  #[test]
  fn chrome_key_control_ids_remain_stable_across_common_state_changes() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );

    let ctx = egui::Context::default();
    let regular = egui::vec2(800.0, 600.0);
    let compact = egui::vec2(500.0, 600.0);

    // Baseline (non-compact, not loading).
    begin_frame_with_screen_size(&ctx, regular, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let baseline = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );

    // Toggle loading (Reload → Stop loading). This should not perturb ids for unrelated controls.
    app.active_tab_mut().unwrap().loading = true;
    begin_frame_with_screen_size(&ctx, regular, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let loading_ids = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );
    assert_eq!(
      baseline, loading_ids,
      "expected key chrome control ids to remain stable when loading toggles"
    );

    // Switch to compact layout (zoom buttons are removed). Key control ids should remain stable.
    begin_frame_with_screen_size(&ctx, compact, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let compact_ids = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );
    assert_eq!(
      baseline, compact_ids,
      "expected key chrome control ids to remain stable when switching to compact layout"
    );

    // In compact mode, enabling a non-default zoom inserts a reset pill. Ensure ids remain stable.
    app.active_tab_mut().unwrap().zoom = 1.25;
    begin_frame_with_screen_size(&ctx, compact, Vec::new());
    let _ = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    let compact_zoom_ids = (
      expect_temp_id(&ctx, "chrome_tab_strip_new_tab_button_id"),
      expect_temp_id(&ctx, "chrome_menu_button_id"),
      expect_temp_id(&ctx, "chrome_appearance_button_id"),
    );
    assert_eq!(
      baseline, compact_zoom_ids,
      "expected key chrome control ids to remain stable when compact zoom controls toggle"
    );
  }

  #[test]
  fn enter_activates_downloads_button_when_focused() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 0: render once to capture widget ids from `store_test_id`.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let downloads_id = expect_temp_id(&ctx, "chrome_downloads_button_id");

    // Frame 1: move focus to the downloads button.
    ctx.memory_mut(|mem| mem.request_focus(downloads_id));
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(downloads_id)),
      "expected downloads button to have focus"
    );

    // Frame 2: press Enter; should activate like a primary click.
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleDownloadsPanel)),
      "expected ChromeAction::ToggleDownloadsPanel, got {actions:?}"
    );
  }

  #[test]
  fn space_activates_bookmark_star_when_focused() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    // Frame 0: render once to capture widget ids from `store_test_id`.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let bookmark_id = expect_temp_id(&ctx, "chrome_bookmark_star_id");

    // Frame 1: move focus to the bookmark star.
    ctx.memory_mut(|mem| mem.request_focus(bookmark_id));
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(bookmark_id)),
      "expected bookmark star to have focus"
    );

    // Frame 2: press Space; should activate like a primary click.
    begin_frame(&ctx, vec![key_press(egui::Key::Space)]);
    let actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::ToggleBookmarkForActiveTab)),
      "expected ChromeAction::ToggleBookmarkForActiveTab, got {actions:?}"
    );
  }

  #[test]
  fn tab_focus_traversal_in_nav_row_is_left_to_right() {
    // Expected focus traversal order matches the visual left-to-right order of the main toolbar
    // row (navigation, zoom controls, address bar cluster).
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.can_go_back = true;
    tab.can_go_forward = true;
    app.push_tab(tab, true);
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    // Frame 0: render once to capture the widget ids from `store_test_id`.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let order = nav_row_tab_order(&ctx);

    // Frame 1: focus the first widget (back button).
    ctx.memory_mut(|mem| mem.request_focus(order[0]));
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(order[0])),
      "expected initial focus on back button"
    );

    // Subsequent frames: press Tab and ensure focus advances in the expected order.
    for (idx, expected) in order.iter().enumerate().skip(1) {
      begin_frame(&ctx, vec![key_press(egui::Key::Tab)]);
      let _ = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        Some(&bookmarks),
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  #[test]
  fn shift_tab_focus_traversal_in_nav_row_is_right_to_left() {
    // Mirror `tab_focus_traversal_in_nav_row_is_left_to_right`, but traverse backwards (Shift+Tab)
    // from the right-most toolbar control.
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.can_go_back = true;
    tab.can_go_forward = true;
    app.push_tab(tab, true);
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    // Frame 0: render once to capture the widget ids from `store_test_id`.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let order = nav_row_tab_order(&ctx);
    let reverse: Vec<_> = order.iter().rev().copied().collect();

    // Frame 1: focus the last widget (appearance button).
    ctx.memory_mut(|mem| mem.request_focus(reverse[0]));
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(reverse[0])),
      "expected initial focus on appearance button"
    );

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

    // Subsequent frames: press Shift+Tab and ensure focus moves in reverse order.
    for (idx, expected) in reverse.iter().enumerate().skip(1) {
      begin_frame(&ctx, vec![shift_tab_press()]);
      let _ = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        Some(&bookmarks),
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Shift+Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  #[test]
  fn tab_focus_traversal_in_nav_row_is_left_to_right_in_compact_mode() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.can_go_back = true;
    tab.can_go_forward = true;
    app.push_tab(tab, true);
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let screen_size = egui::vec2(500.0, 600.0);

    // Frame 0: render once to capture the widget ids from `store_test_id`.
    begin_frame_with_screen_size(&ctx, screen_size, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let order = nav_row_tab_order_compact(&ctx);

    // Frame 1: focus the first widget (back button).
    ctx.memory_mut(|mem| mem.request_focus(order[0]));
    begin_frame_with_screen_size(&ctx, screen_size, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(order[0])),
      "expected initial focus on back button"
    );

    for (idx, expected) in order.iter().enumerate().skip(1) {
      begin_frame_with_screen_size(&ctx, screen_size, vec![key_press(egui::Key::Tab)]);
      let _ = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        Some(&bookmarks),
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  #[test]
  fn shift_tab_focus_traversal_in_nav_row_is_right_to_left_in_compact_mode() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let mut tab = BrowserTabState::new(tab_id, "https://example.com/".to_string());
    tab.can_go_back = true;
    tab.can_go_forward = true;
    app.push_tab(tab, true);
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let screen_size = egui::vec2(500.0, 600.0);

    // Frame 0: render once to capture the widget ids from `store_test_id`.
    begin_frame_with_screen_size(&ctx, screen_size, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let order = nav_row_tab_order_compact(&ctx);
    let reverse: Vec<_> = order.iter().rev().copied().collect();

    // Frame 1: focus the last widget (appearance button).
    ctx.memory_mut(|mem| mem.request_focus(reverse[0]));
    begin_frame_with_screen_size(&ctx, screen_size, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(reverse[0])),
      "expected initial focus on appearance button"
    );

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

    for (idx, expected) in reverse.iter().enumerate().skip(1) {
      begin_frame_with_screen_size(&ctx, screen_size, vec![shift_tab_press()]);
      let _ = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        Some(&bookmarks),
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      let focused = order
        .iter()
        .copied()
        .find(|id| ctx.memory(|mem| mem.has_focus(*id)));
      assert_eq!(
        focused,
        Some(*expected),
        "unexpected focus after Shift+Tab step {idx}; expected {expected:?}, got {focused:?}"
      );
    }
  }

  fn click_menu_item(
    ctx: &egui::Context,
    app: &mut BrowserAppState,
    bookmarks: Option<&BookmarkStore>,
    item_rect_key: &'static str,
  ) -> Vec<ChromeAction> {
    // Frame 1: layout, capture the menu button rect.
    begin_frame(ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(ctx, app, bookmarks, true, |_| None);
    let _ = ctx.end_frame();
    let menu_button_rect = expect_temp_rect(ctx, "chrome_menu_button_rect");

    // Frame 2: click the menu button, capture the menu item rect.
    begin_frame(ctx, left_click_at(menu_button_rect.center()));
    let _ = chrome_ui_with_bookmarks(ctx, app, bookmarks, true, |_| None);
    let _ = ctx.end_frame();
    let item_rect = expect_temp_rect(ctx, item_rect_key);

    // Frame 3: click the menu item and return emitted actions.
    begin_frame(ctx, left_click_at(item_rect.center()));
    let actions = chrome_ui_with_bookmarks(ctx, app, bookmarks, true, |_| None);
    let _ = ctx.end_frame();
    actions
  }

  fn open_menu_for_accesskit(
    ctx: &egui::Context,
    app: &mut BrowserAppState,
    bookmarks: Option<&BookmarkStore>,
  ) -> egui::FullOutput {
    // Frame 0: layout, capture the menu button rect.
    begin_frame(ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(ctx, app, bookmarks, true, |_| None);
    let _ = ctx.end_frame();
    let menu_button_rect = expect_temp_rect(ctx, "chrome_menu_button_rect");

    // Frame 1: click the menu button to open the popup and capture AccessKit output.
    begin_frame(ctx, left_click_at(menu_button_rect.center()));
    let _ = chrome_ui_with_bookmarks(ctx, app, bookmarks, true, |_| None);
    ctx.end_frame()
  }

  fn accesskit_checkbox_checked_state(
    output: &egui::FullOutput,
    name: &str,
  ) -> Option<accesskit::CheckedState> {
    let update = output.platform_output.accesskit_update.as_ref()?;
    update.nodes.iter().find_map(|(_id, node)| {
      if node.role() != accesskit::Role::CheckBox {
        return None;
      }
      let node_name = node.name().unwrap_or("").trim();
      if node_name != name {
        return None;
      }
      a11y_test_util::accesskit_node_checked(node)
    })
  }

  #[test]
  fn chrome_menu_toggle_bookmark_emits_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let actions = click_menu_item(
      &ctx,
      &mut app,
      Some(&bookmarks),
      "chrome_menu_item_toggle_bookmark_rect",
    );
    assert!(
      matches!(
        actions.as_slice(),
        [ChromeAction::ToggleBookmarkForActiveTab]
      ),
      "expected ChromeAction::ToggleBookmarkForActiveTab, got {actions:?}"
    );
  }

  #[test]
  fn chrome_menu_toggle_bookmarks_manager_emits_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let actions = click_menu_item(
      &ctx,
      &mut app,
      Some(&bookmarks),
      "chrome_menu_item_toggle_bookmarks_manager_rect",
    );
    assert!(
      matches!(actions.as_slice(), [ChromeAction::ToggleBookmarksManager]),
      "expected ChromeAction::ToggleBookmarksManager, got {actions:?}"
    );
  }

  #[test]
  fn chrome_menu_accesskit_announces_show_history_when_closed() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "https://example.com/".to_string()),
      true,
    );
    app.chrome.history_panel_open = false;
    let bookmarks = BookmarkStore::default();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let output = open_menu_for_accesskit(&ctx, &mut app, Some(&bookmarks));
    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      nodes
        .iter()
        .any(|n| n.role == "CheckBox" && n.name == "Show history panel"),
      "expected \"Show history panel\" to appear as a CheckBox in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
    assert_eq!(
      accesskit_checkbox_checked_state(&output, "Show history panel"),
      Some(accesskit::CheckedState::False),
      "expected \"Show history panel\" checkbox to be unchecked when the panel is closed.\n\nsnapshot:\n{snapshot}"
    );
    assert!(
      !nodes.iter().any(|n| n.name == "Hide history panel"),
      "expected \"Hide history panel\" not to appear in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn chrome_menu_accesskit_announces_hide_history_when_open() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "https://example.com/".to_string()),
      true,
    );
    app.chrome.history_panel_open = true;
    let bookmarks = BookmarkStore::default();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let output = open_menu_for_accesskit(&ctx, &mut app, Some(&bookmarks));
    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      nodes
        .iter()
        .any(|n| n.role == "CheckBox" && n.name == "Hide history panel"),
      "expected \"Hide history panel\" to appear as a CheckBox in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
    assert_eq!(
      accesskit_checkbox_checked_state(&output, "Hide history panel"),
      Some(accesskit::CheckedState::True),
      "expected \"Hide history panel\" checkbox to be checked when the panel is open.\n\nsnapshot:\n{snapshot}"
    );
    assert!(
      !nodes.iter().any(|n| n.name == "Show history panel"),
      "expected \"Show history panel\" not to appear in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn chrome_menu_accesskit_announces_show_bookmarks_manager_when_closed() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "https://example.com/".to_string()),
      true,
    );
    app.chrome.bookmarks_manager_open = false;
    let bookmarks = BookmarkStore::default();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let output = open_menu_for_accesskit(&ctx, &mut app, Some(&bookmarks));
    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      nodes
        .iter()
        .any(|n| n.role == "CheckBox" && n.name == "Show bookmarks manager"),
      "expected \"Show bookmarks manager\" to appear as a CheckBox in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
    assert_eq!(
      accesskit_checkbox_checked_state(&output, "Show bookmarks manager"),
      Some(accesskit::CheckedState::False),
      "expected \"Show bookmarks manager\" checkbox to be unchecked when the panel is closed.\n\nsnapshot:\n{snapshot}"
    );
    assert!(
      !nodes.iter().any(|n| n.name == "Hide bookmarks manager"),
      "expected \"Hide bookmarks manager\" not to appear in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn chrome_menu_accesskit_announces_hide_bookmarks_manager_when_open() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "https://example.com/".to_string()),
      true,
    );
    app.chrome.bookmarks_manager_open = true;
    let bookmarks = BookmarkStore::default();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let output = open_menu_for_accesskit(&ctx, &mut app, Some(&bookmarks));
    let nodes = a11y_test_util::accesskit_named_roles_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_named_roles_pretty_json_from_full_output(&output);

    assert!(
      nodes
        .iter()
        .any(|n| n.role == "CheckBox" && n.name == "Hide bookmarks manager"),
      "expected \"Hide bookmarks manager\" to appear as a CheckBox in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
    assert_eq!(
      accesskit_checkbox_checked_state(&output, "Hide bookmarks manager"),
      Some(accesskit::CheckedState::True),
      "expected \"Hide bookmarks manager\" checkbox to be checked when the panel is open.\n\nsnapshot:\n{snapshot}"
    );
    assert!(
      !nodes.iter().any(|n| n.name == "Show bookmarks manager"),
      "expected \"Show bookmarks manager\" not to appear in AccessKit output.\n\nsnapshot:\n{snapshot}"
    );
  }

  #[test]
  fn chrome_menu_toggle_history_emits_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let actions = click_menu_item(
      &ctx,
      &mut app,
      Some(&bookmarks),
      "chrome_menu_item_toggle_history_rect",
    );
    assert!(
      matches!(actions.as_slice(), [ChromeAction::ToggleHistoryPanel]),
      "expected ChromeAction::ToggleHistoryPanel, got {actions:?}"
    );
  }

  #[test]
  fn chrome_menu_open_clear_browsing_data_dialog_emits_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let actions = click_menu_item(
      &ctx,
      &mut app,
      Some(&bookmarks),
      "chrome_menu_item_open_clear_browsing_data_rect",
    );
    assert!(
      matches!(
        actions.as_slice(),
        [ChromeAction::OpenClearBrowsingDataDialog]
      ),
      "expected ChromeAction::OpenClearBrowsingDataDialog, got {actions:?}"
    );
  }

  #[test]
  fn address_bar_display_mode_shows_full_host_including_subdomains() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://accounts.google.com/path".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let texts = collect_text_strings(&output.shapes);
    assert!(
      texts
        .iter()
        .any(|text| text.contains("accounts.google.com")),
      "expected address bar display to contain the subdomain prefix; found texts: {texts:?}"
    );
  }

  #[test]
  fn chrome_menu_open_home_url_dialog_emits_action() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    let actions = click_menu_item(
      &ctx,
      &mut app,
      Some(&bookmarks),
      "chrome_menu_item_open_home_url_dialog_rect",
    );
    assert!(
      matches!(actions.as_slice(), [ChromeAction::OpenHomeUrlDialog]),
      "expected ChromeAction::OpenHomeUrlDialog, got {actions:?}"
    );
  }

  #[test]
  fn address_bar_bookmark_star_is_keyboard_activatable() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let bookmarks = BookmarkStore::default();
    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores test ids.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      Some(&bookmarks),
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let bookmark_id = expect_temp_id(&ctx, "chrome_bookmark_star_id");

    for key in [egui::Key::Enter, egui::Key::Space] {
      ctx.memory_mut(|mem| mem.request_focus(bookmark_id));
      begin_frame(&ctx, vec![key_press(key)]);
      let actions = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        Some(&bookmarks),
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      assert!(
        actions
          .iter()
          .any(|action| matches!(action, ChromeAction::ToggleBookmarkForActiveTab)),
        "expected ChromeAction::ToggleBookmarkForActiveTab for key={key:?}, got {actions:?}"
      );
    }
  }

  #[test]
  fn address_bar_downloads_button_is_keyboard_activatable() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores test ids.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let downloads_id = expect_temp_id(&ctx, "chrome_downloads_button_id");

    for key in [egui::Key::Enter, egui::Key::Space] {
      ctx.memory_mut(|mem| mem.request_focus(downloads_id));
      begin_frame(&ctx, vec![key_press(key)]);
      let actions = chrome_ui_with_bookmarks(
        &ctx,
        &mut app,
        None,
        ctx.wants_keyboard_input(),
        true,
        |_| None,
      );
      let _ = ctx.end_frame();

      assert!(
        actions
          .iter()
          .any(|action| matches!(action, ChromeAction::ToggleDownloadsPanel)),
        "expected ChromeAction::ToggleDownloadsPanel for key={key:?}, got {actions:?}"
      );
    }
  }

  fn assert_address_bar_select_all(ctx: &egui::Context, address_bar_id: egui::Id, end: usize) {
    let state =
      egui::text_edit::TextEditState::load(ctx, address_bar_id).expect("expected TextEdit state");
    let range = state
      .ccursor_range()
      .expect("expected address bar to have a cursor range");
    assert_eq!(range.primary.index, 0);
    assert_eq!(range.secondary.index, end);
  }

  #[test]
  fn address_bar_paste_replaces_selection_and_opens_omnibox() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/path?x=1#y".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 1: focus the address bar so it is ready to accept input.
    app.chrome.request_focus_address_bar = true;
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let address_bar_id = expect_temp_id(&ctx, "chrome_address_bar_text_edit_id");
    assert!(
      app.chrome.address_bar_has_focus,
      "expected address bar to have focus"
    );
    assert!(
      app.chrome.address_bar_editing,
      "expected address bar to be in editing mode"
    );

    // Ensure the existing URL is selected so Paste should replace it (not append/insert).
    let end = app.chrome.address_bar_text.chars().count();
    let mut state = egui::text_edit::TextEditState::load(&ctx, address_bar_id).unwrap_or_default();
    state.set_ccursor_range(Some(egui::text::CCursorRange::two(
      egui::text::CCursor::new(0),
      egui::text::CCursor::new(end),
    )));
    state.store(&ctx, address_bar_id);
    assert_address_bar_select_all(&ctx, address_bar_id, end);

    // Frame 2: paste should replace the selection and count as user input (open omnibox).
    begin_frame(&ctx, vec![egui::Event::Paste("example.com".to_string())]);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert_eq!(app.chrome.address_bar_text, "example.com");
    assert!(
      app.chrome.omnibox.open,
      "expected omnibox suggestions to open after pasting into the address bar"
    );
  }

  #[test]
  fn address_bar_display_mode_enter_enters_editing_and_selects_all() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores test ids.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let display_id = expect_temp_id(&ctx, "chrome_address_bar_display_id");
    let address_bar_id = expect_temp_id(&ctx, "chrome_address_bar_text_edit_id");
    let end = app.chrome.address_bar_text.chars().count();

    ctx.memory_mut(|mem| mem.request_focus(display_id));
    begin_frame(&ctx, vec![key_press(egui::Key::Enter)]);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      app.chrome.address_bar_has_focus,
      "expected address bar to have focus"
    );
    assert!(
      app.chrome.address_bar_editing,
      "expected address bar to be editing"
    );
    assert_address_bar_select_all(&ctx, address_bar_id, end);
  }

  #[test]
  fn address_bar_display_mode_space_enters_editing_and_selects_all() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    app.push_tab(
      BrowserTabState::new(tab_id, "https://example.com/".to_string()),
      true,
    );
    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores test ids.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    let display_id = expect_temp_id(&ctx, "chrome_address_bar_display_id");
    let address_bar_id = expect_temp_id(&ctx, "chrome_address_bar_text_edit_id");
    let end = app.chrome.address_bar_text.chars().count();

    ctx.memory_mut(|mem| mem.request_focus(display_id));
    begin_frame(&ctx, vec![key_press(egui::Key::Space)]);
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();

    assert!(
      app.chrome.address_bar_has_focus,
      "expected address bar to have focus"
    );
    assert!(
      app.chrome.address_bar_editing,
      "expected address bar to be editing"
    );
    assert_address_bar_select_all(&ctx, address_bar_id, end);
  }

  #[test]
  fn address_bar_display_mode_context_menu_copy_url_sets_clipboard_text() {
    let mut app = BrowserAppState::new();
    let tab_id = TabId(1);
    let url = "https://example.com/path".to_string();
    app.push_tab(BrowserTabState::new(tab_id, url.clone()), true);
    let ctx = egui::Context::default();

    // Frame 0: render once so the chrome stores test rects.
    begin_frame(&ctx, Vec::new());
    let _ = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    let address_bar_rect = expect_temp_rect(&ctx, "chrome_address_bar_rect");

    // Frame 1: right-click the address bar to open the context menu.
    begin_frame(&ctx, right_click_at(address_bar_rect.center()));
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let _ = ctx.end_frame();
    assert!(
      !app.chrome.address_bar_has_focus && !app.chrome.address_bar_editing,
      "expected right-click to keep the address bar in display mode"
    );

    // Frame 2: render again so the popup contents appear.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    let copy_url_text_pos = find_text_pos(&output.shapes, "Copy URL").unwrap_or_else(|| {
      let texts = collect_text_strings(&output.shapes);
      panic!("expected Copy URL menu item; found texts: {texts:?}");
    });

    // Frame 3: click the "Copy URL" menu item.
    begin_frame(
      &ctx,
      left_click_at(copy_url_text_pos + egui::vec2(1.0, 1.0)),
    );
    let _actions = chrome_ui_with_bookmarks(
      &ctx,
      &mut app,
      None,
      ctx.wants_keyboard_input(),
      true,
      |_| None,
    );
    let output = ctx.end_frame();

    assert_eq!(output.platform_output.copied_text, url);
  }

  #[test]
  fn tab_context_menu_duplicate_emits_duplicate_tab_action() {
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

    // Frame 0: render once to obtain stable tab rects from the tab strip test layout.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    let (_strip_rect, tab_rects) = super::tab_strip::load_test_layout(&ctx)
      .expect("expected tab strip layout metadata in egui context");
    let tab_a_rect = tab_rects
      .first()
      .copied()
      .expect("expected at least one tab rect");

    // Frame 1: right-click the first tab to open the context menu.
    begin_frame(&ctx, right_click_at(tab_a_rect.center()));
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 2: render again so the popup contents appear.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let duplicate_text_pos = find_text_pos(&output.shapes, "Duplicate Tab").unwrap_or_else(|| {
      let texts = collect_text_strings(&output.shapes);
      panic!("expected Duplicate Tab menu item; found texts: {texts:?}");
    });

    // Frame 3: click the "Duplicate Tab" menu item.
    begin_frame(
      &ctx,
      left_click_at(duplicate_text_pos + egui::vec2(1.0, 1.0)),
    );
    let actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      actions
        .iter()
        .any(|action| matches!(action, ChromeAction::DuplicateTab(id) if *id == tab_a)),
      "expected ChromeAction::DuplicateTab({tab_a:?}), got {actions:?}"
    );
  }

  #[test]
  fn escape_closing_tab_context_menu_restores_focus_to_opener_tab() {
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

    // Frame 0: open the context menu and render once so the menu populates its focus id list.
    app.chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
      tab_id: tab_a,
      anchor_points: (50.0, 50.0),
      opener_focus: Some(UiFocusToken(tab_a.0)),
    });
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Grab the menu's known focusable widget ids and focus one to simulate keyboard navigation
    // inside the popup.
    let menu_id = egui::Id::new(("tab_context_menu", tab_a));
    let popup_focus_ids = ctx
      .data(|d| d.get_temp::<Vec<egui::Id>>(menu_id.with("popup_focus_ids")))
      .unwrap_or_default();
    let first_focus_id = *popup_focus_ids
      .first()
      .expect("expected tab context menu to store popup focus ids");
    ctx.memory_mut(|mem| mem.request_focus(first_focus_id));

    // Frame 1: press Escape to close. Focus should return to the tab strip opener widget.
    begin_frame(&ctx, vec![key_press(egui::Key::Escape)]);
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();
    assert!(
      app.chrome.open_tab_context_menu.is_none(),
      "expected tab context menu to be closed"
    );

    let opener_id = super::tab_strip::tab_strip_tab_widget_id(tab_a);
    assert!(
      ctx.memory(|mem| mem.has_focus(opener_id)),
      "expected focus to be restored to tab opener widget after Escape"
    );
  }

  #[test]
  fn tab_context_menu_opens_via_shift_f10_and_focuses_first_item_with_accesskit_labels() {
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

    // Frame 0: render once so the tab widgets exist.
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 1: focus the first tab.
    let tab_id = super::tab_strip::tab_strip_tab_widget_id(tab_a);
    ctx.memory_mut(|mem| mem.request_focus(tab_id));
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(tab_id)),
      "expected invoking tab to have focus before opening the menu"
    );

    // Frame 2: inject Shift+F10 to open the context menu via keyboard.
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    raw.modifiers.shift = true;
    raw.events = vec![egui::Event::Key {
      key: egui::Key::F10,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }];
    ctx.begin_frame(raw);
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    // Frame 3: render again so the popup contents are present and AccessKit can see them.
    ctx.enable_accesskit();
    begin_frame(&ctx, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let output = ctx.end_frame();

    let reload_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new("test_tab_context_menu_reload_id")))
      .expect("expected test_tab_context_menu_reload_id to be stored");
    assert!(
      ctx.memory(|mem| mem.has_focus(reload_id)),
      "expected focus to move to the first menu item when opened via keyboard"
    );

    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    for expected in ["Reload tab", "Duplicate tab", "Close tab"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in tab context menu output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
      );
    }
  }

  #[test]
  fn tab_context_menu_is_constrained_to_screen_rect() {
    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut tab_a_state = BrowserTabState::new(tab_a, "about:newtab".to_string());
    // Pinned tabs avoid extra group-related menu items, keeping the popup small enough for the
    // constrained screen size used in this regression test.
    tab_a_state.pinned = true;
    app.push_tab(tab_a_state, true);
    app.push_tab(
      BrowserTabState::new(tab_b, "about:newtab".to_string()),
      false,
    );

    let ctx = egui::Context::default();
    let screen_size = egui::vec2(320.0, 200.0);
    let screen_rect = egui::Rect::from_min_size(egui::Pos2::new(0.0, 0.0), screen_size);

    // Anchor the menu at the bottom-right corner so it would overflow without constraint.
    app.chrome.open_tab_context_menu = Some(OpenTabContextMenuState {
      tab_id: tab_a,
      anchor_points: (screen_rect.right() - 1.0, screen_rect.bottom() - 1.0),
      opener_focus: None,
    });
    app.chrome.tab_context_menu_rect = None;

    begin_frame_with_screen_size(&ctx, screen_size, Vec::new());
    let _actions = chrome_ui(&ctx, &mut app, ctx.wants_keyboard_input(), true, |_| None);
    let _ = ctx.end_frame();

    let (min_x, min_y, max_x, max_y) = app
      .chrome
      .tab_context_menu_rect
      .expect("expected tab context menu rect");
    let menu_rect = egui::Rect::from_min_max(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));

    let eps = 0.01;
    assert!(
      menu_rect.left() >= screen_rect.left() - eps,
      "expected menu left ({}) >= screen left ({})",
      menu_rect.left(),
      screen_rect.left()
    );
    assert!(
      menu_rect.top() >= screen_rect.top() - eps,
      "expected menu top ({}) >= screen top ({})",
      menu_rect.top(),
      screen_rect.top()
    );
    assert!(
      menu_rect.right() <= screen_rect.right() + eps,
      "expected menu right ({}) <= screen right ({})",
      menu_rect.right(),
      screen_rect.right()
    );
    assert!(
      menu_rect.bottom() <= screen_rect.bottom() + eps,
      "expected menu bottom ({}) <= screen bottom ({})",
      menu_rect.bottom(),
      screen_rect.bottom()
    );
  }
}
