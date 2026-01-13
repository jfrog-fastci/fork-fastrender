//! Browser UI session persistence (windows + tabs + active selection).
//!
//! Kept behind the `browser_ui` feature gate so core renderer builds remain lean.

use crate::ui::about_pages;
use crate::ui::appearance;
use crate::ui::appearance::AppearanceSettings;
use crate::ui::browser_app::{
  BrowserAppState, DownloadStatus, TabGroupColor, TabGroupId, CLOSED_TAB_STACK_CAPACITY,
};
use crate::ui::protocol_limits;
use crate::ui::protocol_limits::MAX_TITLE_BYTES;
use crate::ui::untrusted::clamp_untrusted_utf8;
use crate::ui::validate_user_navigation_url_scheme;
use crate::ui::zoom;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const SESSION_ENV_PATH: &str = "FASTR_BROWSER_SESSION_PATH";
const SESSION_FILE_NAME: &str = "fastrender_session.json";
const SESSION_VERSION: u32 = 2;
/// Maximum on-disk session payload size that `load_session` will read.
///
/// The persisted session JSON should be small (URLs, zoom levels, window geometry). If the file is
/// corrupted or replaced with a huge payload, unbounded reads can cause excessive memory usage or
/// long stalls at startup.
///
/// When this limit is exceeded, we refuse to read/parse the session and attempt to fall back to
/// the `.bak` backup file (if present).
const MAX_SESSION_FILE_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB

const MAX_WINDOW_DIM_PX: i64 = 16_384;
const MAX_WINDOW_POS_ABS_PX: i64 = 1_000_000;
const FALLBACK_WINDOW_WIDTH_PX: i64 = 1_024;
const FALLBACK_WINDOW_HEIGHT_PX: i64 = 768;
const MAX_SCROLL_CSS: f32 = 1e9;
const MAX_PERSISTED_DOWNLOADS: usize = 200;

// -----------------------------------------------------------------------------
// Session sanitization safety limits
// -----------------------------------------------------------------------------
//
// Session files are treated as untrusted input: even when we enforce an on-disk file size limit,
// a small JSON can still contain pathological structures (e.g. thousands of windows/tabs or huge
// strings). These limits keep startup work bounded and prevent large allocations.

/// Maximum number of windows restored from a session file.
const MAX_SESSION_WINDOWS: usize = 32;

/// Maximum number of tabs restored per window.
const MAX_SESSION_TABS_PER_WINDOW: usize = 256;

/// Maximum number of tab groups restored per window.
const MAX_SESSION_TAB_GROUPS_PER_WINDOW: usize = 64;

/// Maximum UTF-8 bytes retained for URLs in the session file.
///
/// Keep this aligned with the UI↔worker protocol string limits.
const MAX_SESSION_URL_BYTES: usize = protocol_limits::MAX_URL_BYTES;

/// Maximum UTF-8 bytes retained for tab group titles stored in the session file.
const MAX_SESSION_GROUP_TITLE_BYTES: usize = 256;
fn default_did_exit_cleanly() -> bool {
  true
}

fn is_true(value: &bool) -> bool {
  *value
}

fn is_false(value: &bool) -> bool {
  !*value
}

fn is_zero_u32(value: &u32) -> bool {
  *value == 0
}

fn is_default_tab_group_color(value: &TabGroupColor) -> bool {
  *value == TabGroupColor::default()
}

fn default_home_url() -> String {
  about_pages::ABOUT_NEWTAB.to_string()
}

fn is_default_home_url(url: &String) -> bool {
  url == about_pages::ABOUT_NEWTAB
}

fn default_show_menu_bar_for_platform(is_macos: bool) -> bool {
  // On macOS, native apps typically use the system menu bar rather than an in-window menu bar.
  // Default to hiding the egui menu bar for a more platform-native feel.
  !is_macos
}

fn default_show_menu_bar() -> bool {
  default_show_menu_bar_for_platform(cfg!(target_os = "macos"))
}

fn is_default_show_menu_bar(value: &bool) -> bool {
  *value == default_show_menu_bar()
}

fn default_tab_group_title() -> String {
  "Group".to_string()
}

fn is_default_tab_group_title(value: &String) -> bool {
  value == "Group"
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSessionTab {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub zoom: Option<f32>,
  /// Viewport/document scroll offset in CSS pixels.
  ///
  /// `None` means "unknown / top" and is omitted from the serialized JSON for backwards
  /// compatibility and cleanliness.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub scroll_css: Option<(f32, f32)>,
  /// Whether this tab is pinned in the tab strip.
  #[serde(default, skip_serializing_if = "is_false")]
  pub pinned: bool,
  /// Optional tab group membership, represented as an index into [`BrowserSessionWindow::tab_groups`].
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub group: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSessionClosedTab {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Whether this tab was pinned in the tab strip.
  #[serde(default, skip_serializing_if = "is_false")]
  pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSessionTabGroup {
  #[serde(
    default = "default_tab_group_title",
    skip_serializing_if = "is_default_tab_group_title"
  )]
  pub title: String,
  #[serde(default, skip_serializing_if = "is_default_tab_group_color")]
  pub color: TabGroupColor,
  #[serde(default, skip_serializing_if = "is_false")]
  pub collapsed: bool,
}

impl BrowserSessionTabGroup {
  fn sanitized(mut self) -> Self {
    let trimmed = self.title.trim();
    let truncated = truncate_utf8_to_max_bytes(trimmed, MAX_SESSION_GROUP_TITLE_BYTES).trim();
    self.title = if truncated.is_empty() {
      default_tab_group_title()
    } else {
      truncated.to_string()
    };
    self
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserSessionDownload {
  #[serde(default)]
  pub url: String,
  #[serde(default)]
  pub file_name: String,
  #[serde(default)]
  pub path: PathBuf,
  #[serde(default)]
  pub status: BrowserSessionDownloadStatus,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserSessionDownloadStatus {
  Completed,
  Failed,
  Cancelled,
  InProgress,
  #[serde(other)]
  Unknown,
}

impl BrowserSessionDownloadStatus {
  fn sanitized(self) -> Self {
    match self {
      Self::InProgress | Self::Unknown => Self::Cancelled,
      other => other,
    }
  }
}

impl Default for BrowserSessionDownloadStatus {
  fn default() -> Self {
    Self::Cancelled
  }
}

impl BrowserSessionDownload {
  fn from_app_state(entry: &crate::ui::DownloadEntry) -> Self {
    let (status, error) = match &entry.status {
      DownloadStatus::Completed => (BrowserSessionDownloadStatus::Completed, None),
      DownloadStatus::Cancelled => (BrowserSessionDownloadStatus::Cancelled, None),
      DownloadStatus::InProgress { .. } => (BrowserSessionDownloadStatus::InProgress, None),
      DownloadStatus::Failed { error } => (BrowserSessionDownloadStatus::Failed, Some(error.clone())),
    };

    Self {
      url: entry.url.clone(),
      file_name: entry.file_name.clone(),
      path: entry.path.clone(),
      status,
      error,
    }
    .sanitized()
  }

  fn sanitized(mut self) -> Self {
    self.url = self.url.trim().to_string();

    let file_name = self.file_name.trim();
    if file_name.is_empty() {
      self.file_name = self
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "download".to_string());
    } else {
      self.file_name = file_name.to_string();
    }

    self.status = self.status.sanitized();
    if self.status != BrowserSessionDownloadStatus::Failed {
      self.error = None;
    } else if let Some(err) = self.error.as_mut() {
      *err = err.trim().to_string();
    }
    self
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserWindowState {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub x: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub y: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub width: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub height: Option<i64>,
  #[serde(default, skip_serializing_if = "is_false")]
  pub maximized: bool,
}

impl BrowserWindowState {
  fn is_empty(&self) -> bool {
    self.x.is_none()
      && self.y.is_none()
      && self.width.is_none()
      && self.height.is_none()
      && !self.maximized
  }

  fn sanitized(mut self) -> Self {
    self.x = sanitize_window_pos(self.x);
    self.y = sanitize_window_pos(self.y);
    self.width = sanitize_window_dim(self.width);
    self.height = sanitize_window_dim(self.height);

    if self.maximized && (self.width.is_none() || self.height.is_none()) {
      self.width.get_or_insert(FALLBACK_WINDOW_WIDTH_PX);
      self.height.get_or_insert(FALLBACK_WINDOW_HEIGHT_PX);
    }

    self
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSessionWindow {
  #[serde(default)]
  pub tabs: Vec<BrowserSessionTab>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub downloads: Vec<BrowserSessionDownload>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tab_groups: Vec<BrowserSessionTabGroup>,
  /// Stack of recently closed tabs for "Reopen closed tab" UX.
  ///
  /// Oldest entries first; newest/most-recent entry at the end (LIFO stack semantics).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub closed_tabs: Vec<BrowserSessionClosedTab>,
  #[serde(default)]
  pub active_tab_index: usize,
  #[serde(default, skip_serializing_if = "is_false")]
  pub bookmarks_bar_visible: bool,
  #[serde(
    default = "default_show_menu_bar",
    skip_serializing_if = "is_default_show_menu_bar"
  )]
  pub show_menu_bar: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub window_state: Option<BrowserWindowState>,
}

impl BrowserSessionWindow {
  /// Build a session snapshot from the current windowed UI state model.
  ///
  /// This intentionally stores only lightweight serializable data (URLs, zoom, viewport scroll).
  pub fn from_app_state(app: &BrowserAppState) -> Self {
    let mut tabs = Vec::new();
    let mut tab_groups: Vec<BrowserSessionTabGroup> = Vec::new();
    let mut group_indices: HashMap<TabGroupId, usize> = HashMap::new();
    for tab in &app.tabs {
      let mut url = tab
        .current_url
        .clone()
        .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
      if validate_user_navigation_url_scheme(&url).is_err() {
        url = about_pages::ABOUT_NEWTAB.to_string();
      }

      let scroll_css = {
        let viewport = tab.scroll_state.viewport;
        let x = viewport.x;
        let y = viewport.y;
        if !x.is_finite() || !y.is_finite() {
          None
        } else {
          let x = x.max(0.0);
          let y = y.max(0.0);
          ((x, y) != (0.0, 0.0)).then_some((x, y))
        }
      };
      let pinned = tab.pinned;
      let group = if pinned {
        None
      } else {
        tab.group.and_then(|group_id| {
          if let Some(existing) = group_indices.get(&group_id) {
            return Some(*existing);
          }
          let group_state = app.tab_groups.get(&group_id)?;
          let idx = tab_groups.len();
          tab_groups.push(BrowserSessionTabGroup {
            title: group_state.title.clone(),
            color: group_state.color,
            collapsed: group_state.collapsed,
          });
          group_indices.insert(group_id, idx);
          Some(idx)
        })
      };
      tabs.push(BrowserSessionTab {
        url,
        zoom: Some(tab.zoom),
        scroll_css,
        pinned,
        group,
      });
    }

    let active_tab_index = app
      .active_tab_id()
      .and_then(|id| app.tabs.iter().position(|t| t.id == id))
      .unwrap_or(0);

    let mut downloads = app
      .downloads
      .downloads
      .iter()
      .map(BrowserSessionDownload::from_app_state)
      .collect::<Vec<_>>();
    if downloads.len() > MAX_PERSISTED_DOWNLOADS {
      let overflow = downloads.len() - MAX_PERSISTED_DOWNLOADS;
      downloads.drain(0..overflow);
    }
    let closed_tabs = if CLOSED_TAB_STACK_CAPACITY == 0 || app.closed_tabs.is_empty() {
      Vec::new()
    } else {
      // Keep only the most recent entries, mirroring the in-memory stack behaviour.
      let start = app
        .closed_tabs
        .len()
        .saturating_sub(CLOSED_TAB_STACK_CAPACITY);
      app.closed_tabs[start..]
        .iter()
        .map(|tab| BrowserSessionClosedTab {
          url: tab.url.clone(),
          title: tab.title.clone(),
          pinned: tab.pinned,
        })
        .collect()
    };

    Self {
      tabs,
      downloads,
      tab_groups,
      closed_tabs,
      active_tab_index,
      bookmarks_bar_visible: app.chrome.bookmarks_bar_visible,
      show_menu_bar: app.chrome.show_menu_bar,
      window_state: None,
    }
    .sanitized()
  }

  /// Ensure the window is well-formed and contains only supported URLs.
  pub fn sanitized(mut self) -> Self {
    if self.tabs.is_empty() {
      self.tabs.push(BrowserSessionTab {
        url: about_pages::ABOUT_NEWTAB.to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      });
      self.tab_groups.clear();
      self.active_tab_index = 0;
    }

    // Bound the number of tabs/groups we will process.
    self.tabs.truncate(MAX_SESSION_TABS_PER_WINDOW);
    self.tab_groups.truncate(MAX_SESSION_TAB_GROUPS_PER_WINDOW);

    for tab in &mut self.tabs {
      sanitize_tab(tab);
    }

    // Downloads history:
    // - Ensure all entries have a final status (no InProgress carry-over).
    // - Cap the number of entries so session files remain bounded in size.
    self.downloads = std::mem::take(&mut self.downloads)
      .into_iter()
      .map(|d| d.sanitized())
      .collect();
    if self.downloads.len() > MAX_PERSISTED_DOWNLOADS {
      let overflow = self.downloads.len() - MAX_PERSISTED_DOWNLOADS;
      self.downloads.drain(0..overflow);
    }

    // Sanitize tab group state/membership:
    // - pinned tabs cannot be grouped
    // - out-of-range group indices are dropped
    // - empty groups are pruned and indices remapped
    for tab in &mut self.tabs {
      if tab.pinned {
        tab.group = None;
      }
    }
    let group_len = self.tab_groups.len();
    for tab in &mut self.tabs {
      if tab.group.is_some_and(|idx| idx >= group_len) {
        tab.group = None;
      }
    }
    if !self.tab_groups.is_empty() {
      let mut used = vec![false; self.tab_groups.len()];
      for tab in &self.tabs {
        if let Some(idx) = tab.group {
          if let Some(slot) = used.get_mut(idx) {
            *slot = true;
          }
        }
      }

      if used.iter().all(|u| !*u) {
        self.tab_groups.clear();
        for tab in &mut self.tabs {
          tab.group = None;
        }
      } else {
        let old_groups = std::mem::take(&mut self.tab_groups);
        let mut remap: Vec<Option<usize>> = vec![None; old_groups.len()];
        for (old_idx, group) in old_groups.into_iter().enumerate() {
          if used.get(old_idx).copied().unwrap_or(false) {
            let new_idx = self.tab_groups.len();
            self.tab_groups.push(group.sanitized());
            remap[old_idx] = Some(new_idx);
          }
        }

        for tab in &mut self.tabs {
          if let Some(old_idx) = tab.group {
            tab.group = remap.get(old_idx).and_then(|v| *v);
          }
        }
      }
    }

    self.active_tab_index = self.active_tab_index.min(self.tabs.len().saturating_sub(1));

    // Ensure the active tab is always visible: when the active tab belongs to a collapsed group,
    // force that group to expand.
    if let Some(active) = self.tabs.get(self.active_tab_index) {
      if let Some(group_idx) = active.group {
        if let Some(group) = self.tab_groups.get_mut(group_idx) {
          group.collapsed = false;
        }
      }
    }

    // Sanitize recently closed tabs (reopen-closed-tab stack).
    if CLOSED_TAB_STACK_CAPACITY == 0 {
      self.closed_tabs.clear();
    } else if !self.closed_tabs.is_empty() {
      // Preserve stack semantics by keeping the newest entries (end of the vec).
      let raw = std::mem::take(&mut self.closed_tabs);
      let mut keep_rev: Vec<BrowserSessionClosedTab> =
        Vec::with_capacity(raw.len().min(CLOSED_TAB_STACK_CAPACITY));
      for mut tab in raw.into_iter().rev() {
        if keep_rev.len() >= CLOSED_TAB_STACK_CAPACITY {
          break;
        }
        let trimmed = tab.url.trim();
        let truncated = truncate_utf8_to_max_bytes(trimmed, MAX_SESSION_URL_BYTES).trim();
        if truncated.is_empty() {
          continue;
        }
        if validate_user_navigation_url_scheme(truncated).is_err() {
          continue;
        }
        tab.url = truncated.to_string();
        if let Some(title) = tab.title.as_mut() {
          if title.len() > MAX_TITLE_BYTES {
            *title = clamp_untrusted_utf8(title, MAX_TITLE_BYTES);
          }
        }
        keep_rev.push(tab);
      }
      keep_rev.reverse();
      self.closed_tabs = keep_rev;
    }

    self.window_state = self
      .window_state
      .take()
      .map(|state| state.sanitized())
      .filter(|state| !state.is_empty());

    self
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSession {
  pub version: u32,
  #[serde(
    default = "default_home_url",
    skip_serializing_if = "is_default_home_url"
  )]
  pub home_url: String,
  #[serde(default)]
  pub windows: Vec<BrowserSessionWindow>,
  #[serde(default)]
  pub active_window_index: usize,
  #[serde(default, skip_serializing_if = "AppearanceSettings::is_default")]
  pub appearance: AppearanceSettings,
  /// Whether the previous browser process believes it shut down cleanly.
  ///
  /// This is used as a lightweight crash marker:
  /// - On startup, the windowed UI should autosave `did_exit_cleanly=false` as soon as the session
  ///   is restored so unexpected crashes can be detected on next launch.
  /// - On clean shutdown, the UI should write `did_exit_cleanly=true`.
  ///
  /// When loading a legacy session file that does not contain this field, we default to `true` to
  /// preserve the old semantics (sessions were only written on clean shutdown).
  #[serde(default = "default_did_exit_cleanly", skip_serializing_if = "is_true")]
  pub did_exit_cleanly: bool,
  /// Number of consecutive unclean exits observed across startups.
  ///
  /// This is a crash-loop breaker input: if session restore repeatedly crashes immediately, the
  /// browser can stop auto-restoring tabs after a threshold and start in a safe mode instead.
  ///
  /// Managed by `session_autosave`:
  /// - On startup, when the crash marker flips `did_exit_cleanly=false`, this streak is incremented.
  /// - On clean shutdown, it is reset to 0.
  #[serde(default, skip_serializing_if = "is_zero_u32")]
  pub unclean_exit_streak: u32,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub ui_scale: Option<f32>,
}

impl BrowserSession {
  pub fn single(url: String) -> Self {
    Self {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url,
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized()
  }

  /// Build a session snapshot from a set of windowed UI state models.
  ///
  /// This intentionally stores only lightweight serializable data (URLs, zoom, viewport scroll).
  pub fn from_windows(
    windows: impl IntoIterator<Item = BrowserSessionWindow>,
    active_window_index: usize,
    appearance: AppearanceSettings,
  ) -> Self {
    let ui_scale = appearance::clamp_ui_scale(appearance.ui_scale);
    Self {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: windows.into_iter().collect(),
      active_window_index,
      appearance,
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: (ui_scale != appearance::DEFAULT_UI_SCALE).then_some(ui_scale),
    }
    .sanitized()
  }

  /// Build a session snapshot from the current windowed UI state model.
  ///
  /// This intentionally stores only lightweight serializable data (URLs, zoom, viewport scroll).
  pub fn from_app_state(app: &BrowserAppState) -> Self {
    Self::from_windows(
      [BrowserSessionWindow::from_app_state(app)],
      0,
      app.appearance.clone(),
    )
  }

  /// Ensure the session is well-formed and contains only supported URLs.
  pub fn sanitized(mut self) -> Self {
    self.version = SESSION_VERSION;
    self.appearance = self.appearance.sanitized();

    if self.did_exit_cleanly {
      // Ensure an on-disk "clean" marker always implies a reset streak.
      self.unclean_exit_streak = 0;
    }

    sanitize_url_in_place(&mut self.home_url, about_pages::ABOUT_NEWTAB);

    if self.windows.is_empty() {
      self.windows.push(BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: about_pages::ABOUT_NEWTAB.to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      });
      self.active_window_index = 0;
    }

    self.windows = std::mem::take(&mut self.windows)
      .into_iter()
      .take(MAX_SESSION_WINDOWS)
      .map(|window| window.sanitized())
      .collect();

    self.active_window_index = self
      .active_window_index
      .min(self.windows.len().saturating_sub(1));

    self.ui_scale = self
      .ui_scale
      .map(|raw| appearance::clamp_ui_scale(raw))
      .and_then(|scale| (scale != appearance::DEFAULT_UI_SCALE).then_some(scale));

    // Backwards compatibility: older v2 session files stored chrome UI scale in the legacy
    // top-level `ui_scale` field. The new appearance settings persist UI scale inside
    // `appearance.ui_scale`. If the appearance value is still default, treat the legacy field as
    // the persisted value.
    if (self.appearance.ui_scale - appearance::DEFAULT_UI_SCALE).abs() <= 1e-6 {
      if let Some(scale) = self.ui_scale {
        self.appearance.ui_scale = scale;
      }
    }

    // Keep the legacy field in sync so older browser builds (that only understand `ui_scale`) can
    // still restore the user's configured scaling.
    self.appearance.ui_scale = appearance::clamp_ui_scale(self.appearance.ui_scale);
    self.ui_scale = (self.appearance.ui_scale != appearance::DEFAULT_UI_SCALE)
      .then_some(self.appearance.ui_scale);

    self
  }
}

fn sanitize_tab(tab: &mut BrowserSessionTab) {
  sanitize_url_in_place(&mut tab.url, about_pages::ABOUT_NEWTAB);

  tab.zoom = tab
    .zoom
    .map(|raw| {
      if !raw.is_finite() || raw <= 0.0 {
        zoom::DEFAULT_ZOOM
      } else {
        zoom::clamp_zoom(raw)
      }
    })
    .and_then(|zoom| (zoom != zoom::DEFAULT_ZOOM).then_some(zoom));

  tab.scroll_css = tab.scroll_css.and_then(|(x, y)| {
    if !x.is_finite() || !y.is_finite() {
      return None;
    }
    let x = x.max(0.0).min(MAX_SCROLL_CSS);
    let y = y.max(0.0).min(MAX_SCROLL_CSS);
    ((x, y) != (0.0, 0.0)).then_some((x, y))
  });
}

fn sanitize_url_in_place(url: &mut String, fallback: &str) {
  let trimmed = url.trim();
  let truncated = truncate_utf8_to_max_bytes(trimmed, MAX_SESSION_URL_BYTES).trim();
  if truncated.is_empty() || validate_user_navigation_url_scheme(truncated).is_err() {
    *url = fallback.to_string();
  } else if truncated != url.as_str() {
    *url = truncated.to_string();
  }
}

fn truncate_utf8_to_max_bytes(s: &str, max_bytes: usize) -> &str {
  if s.len() <= max_bytes {
    return s;
  }
  let mut end = max_bytes;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  &s[..end]
}

fn sanitize_window_dim(value: Option<i64>) -> Option<i64> {
  let raw = value?;
  if raw <= 0 {
    return None;
  }
  Some(raw.min(MAX_WINDOW_DIM_PX))
}

fn sanitize_window_pos(value: Option<i64>) -> Option<i64> {
  let raw = value?;
  let abs = raw.checked_abs()?;
  (abs <= MAX_WINDOW_POS_ABS_PX).then_some(raw)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn session_sanitizes_invalid_zoom_values() {
    let session = BrowserSession {
      version: 123,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(f32::NAN),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(f32::INFINITY),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(0.0),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(-1.0),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          // Finite but outside the supported UI range should clamp.
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(0.1),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(999.0),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(2.0),
            scroll_css: None,
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(zoom::DEFAULT_ZOOM),
            scroll_css: None,
            pinned: false,
            group: None,
          },
        ],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    let tabs = &session.windows[0].tabs;

    // Non-finite / <= 0 should fall back to DEFAULT_ZOOM, represented as `None` in the session.
    assert_eq!(tabs[0].zoom, None);
    assert_eq!(tabs[1].zoom, None);
    assert_eq!(tabs[2].zoom, None);
    assert_eq!(tabs[3].zoom, None);

    assert_eq!(tabs[4].zoom, Some(zoom::MIN_ZOOM));
    assert_eq!(tabs[5].zoom, Some(zoom::MAX_ZOOM));
    assert_eq!(tabs[6].zoom, Some(2.0));
    assert_eq!(tabs[7].zoom, None);
  }

  #[test]
  fn session_window_sanitizes_closed_tabs_urls_titles_and_schemes() {
    let long_url = format!(
      "https://example.com/{}",
      "a".repeat(MAX_SESSION_URL_BYTES)
    );
    assert!(
      long_url.len() > MAX_SESSION_URL_BYTES,
      "expected long_url to exceed max bytes"
    );
    let long_title = "x".repeat(MAX_TITLE_BYTES + 32);

    let window = BrowserSessionWindow {
      tabs: vec![BrowserSessionTab {
        url: "about:newtab".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      }],
      downloads: Vec::new(),
      tab_groups: Vec::new(),
      closed_tabs: vec![
        BrowserSessionClosedTab {
          url: "javascript:alert(1)".to_string(),
          title: Some("bad".to_string()),
          pinned: false,
        },
        BrowserSessionClosedTab {
          url: long_url.clone(),
          title: Some(long_title),
          pinned: true,
        },
      ],
      active_tab_index: 0,
      bookmarks_bar_visible: false,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.closed_tabs.len(), 1, "invalid schemes should be dropped");
    let restored = &window.closed_tabs[0];
    assert!(restored.pinned);
    assert!(
      restored.url.len() <= MAX_SESSION_URL_BYTES,
      "closed tab URL should be clamped"
    );
    assert!(
      restored.url.starts_with("https://example.com/"),
      "expected clamped URL to preserve scheme/host, got: {}",
      restored.url
    );
    assert!(
      restored.title.as_ref().is_some_and(|t| t.as_bytes().len() <= MAX_TITLE_BYTES),
      "closed tab title should be clamped"
    );
  }

  #[test]
  fn session_window_snapshots_closed_tabs_from_app_state() {
    let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
    app.closed_tabs = vec![
      crate::ui::ClosedTabState {
        url: "about:blank".to_string(),
        title: Some("Closed".to_string()),
        pinned: true,
      },
      crate::ui::ClosedTabState {
        url: "about:newtab".to_string(),
        title: None,
        pinned: false,
      },
    ];

    let window = BrowserSessionWindow::from_app_state(&app);
    assert_eq!(window.closed_tabs.len(), 2);
    assert_eq!(window.closed_tabs[0].url, "about:blank");
    assert_eq!(window.closed_tabs[0].title.as_deref(), Some("Closed"));
    assert!(window.closed_tabs[0].pinned);
    assert_eq!(window.closed_tabs[1].url, "about:newtab");
    assert_eq!(window.closed_tabs[1].title, None);
    assert!(!window.closed_tabs[1].pinned);
  }

  #[test]
  fn session_omits_default_scroll_from_json() {
    let session = BrowserSession::single("about:newtab".to_string());
    let json = serde_json::to_string(&session).expect("serialize session");
    assert!(
      !json.contains("scroll_css"),
      "expected default scroll to be omitted from JSON, got: {json}"
    );
  }

  #[test]
  fn session_pinned_field_is_omitted_when_false_and_serialized_when_true() {
    let mut session = BrowserSession::single("about:newtab".to_string());
    let json = serde_json::to_string(&session).expect("serialize session");
    assert!(
      !json.contains("\"pinned\""),
      "expected pinned=false to be omitted from JSON, got: {json}"
    );

    session.windows[0].tabs[0].pinned = true;
    let json = serde_json::to_string(&session).expect("serialize session");
    assert!(
      json.contains("\"pinned\":true"),
      "expected pinned=true to be serialized, got: {json}"
    );
  }

  #[test]
  fn show_menu_bar_defaults_off_on_macos_on_elsewhere() {
    assert_eq!(default_show_menu_bar_for_platform(true), false);
    assert_eq!(default_show_menu_bar_for_platform(false), true);
  }

  #[test]
  fn session_roundtrips_show_menu_bar_and_omits_default() {
    let default_value = default_show_menu_bar();
    let mut session = BrowserSession::single("about:newtab".to_string());

    // Default value should be omitted for cleanliness.
    session.windows[0].show_menu_bar = default_value;
    let json =
      serde_json::to_string(&session).expect("serialize session with default show_menu_bar");
    assert!(
      !json.contains("show_menu_bar"),
      "expected default show_menu_bar to be omitted from JSON, got: {json}"
    );

    // Non-default value should be roundtripped and present in JSON.
    session.windows[0].show_menu_bar = !default_value;
    let json =
      serde_json::to_string(&session).expect("serialize session with non-default show_menu_bar");
    assert!(
      json.contains("show_menu_bar"),
      "expected non-default show_menu_bar to be present in JSON, got: {json}"
    );
    let parsed = parse_session_json(&json).expect("parse session JSON");
    assert_eq!(parsed.windows[0].show_menu_bar, !default_value);
  }

  #[test]
  fn session_sanitizes_invalid_scroll_values() {
    let session = BrowserSession {
      version: 123,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((f32::NAN, 1.0)),
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((1.0, f32::INFINITY)),
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((-5.0, -3.0)),
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((-5.0, 25.0)),
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((1e12, 5.0)),
            pinned: false,
            group: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((0.0, 0.0)),
            pinned: false,
            group: None,
          },
        ],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    let tabs = &session.windows[0].tabs;

    // Non-finite scrolls are dropped.
    assert_eq!(tabs[0].scroll_css, None);
    assert_eq!(tabs[1].scroll_css, None);

    // Negatives clamp to 0; (0,0) normalizes to None.
    assert_eq!(tabs[2].scroll_css, None);
    assert_eq!(tabs[3].scroll_css, Some((0.0, 25.0)));

    // Absurdly large scrolls are clamped.
    assert_eq!(tabs[4].scroll_css, Some((MAX_SCROLL_CSS, 5.0)));

    // (0,0) normalizes to None.
    assert_eq!(tabs[5].scroll_css, None);
  }

  #[test]
  fn session_sanitizes_invalid_and_empty_tab_group_references() {
    let window = BrowserSessionWindow {
      tabs: vec![
        // Group 1 (valid).
        BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: Some(1),
        },
        // Out-of-range group index should be dropped.
        BrowserSessionTab {
          url: "about:blank".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: Some(99),
        },
        // Pinned tabs cannot be grouped.
        BrowserSessionTab {
          url: "about:error".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: true,
          group: Some(0),
        },
      ],
      downloads: Vec::new(),
      tab_groups: vec![
        BrowserSessionTabGroup {
          title: "unused".to_string(),
          color: TabGroupColor::Red,
          collapsed: false,
        },
        BrowserSessionTabGroup {
          title: "kept".to_string(),
          color: TabGroupColor::Green,
          collapsed: false,
        },
        BrowserSessionTabGroup {
          title: "also_unused".to_string(),
          color: TabGroupColor::Yellow,
          collapsed: false,
        },
      ],
      closed_tabs: Vec::new(),
      active_tab_index: 0,
      bookmarks_bar_visible: false,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    // After sanitization:
    // - group 0 is dropped (only referenced by the pinned tab, which becomes ungrouped)
    // - group 2 is dropped (no tabs)
    // - group 1 is retained and remapped to index 0
    assert_eq!(window.tab_groups.len(), 1);
    assert_eq!(window.tab_groups[0].title, "kept");
    assert_eq!(window.tabs[0].group, Some(0));
    assert_eq!(window.tabs[1].group, None);
    assert_eq!(window.tabs[2].group, None);
    assert!(window.tabs[2].pinned);
  }

  #[test]
  fn session_expands_collapsed_group_containing_active_tab() {
    let window = BrowserSessionWindow {
      tabs: vec![
        BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: Some(0),
        },
        BrowserSessionTab {
          url: "about:blank".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: Some(0),
        },
      ],
      downloads: Vec::new(),
      tab_groups: vec![BrowserSessionTabGroup {
        title: "g".to_string(),
        color: TabGroupColor::Blue,
        collapsed: true,
      }],
      closed_tabs: Vec::new(),
      active_tab_index: 1,
      bookmarks_bar_visible: false,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.tab_groups.len(), 1);
    assert!(
      !window.tab_groups[0].collapsed,
      "expected collapsed group containing active tab to be expanded"
    );
  }

  #[test]
  fn from_app_state_includes_non_default_zoom() {
    use crate::ui::{BrowserAppState, BrowserTabState, TabId};
    use crate::Point;

    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let mut a = BrowserTabState::new(tab_a, "about:newtab".to_string());
    a.zoom = 1.5;
    a.scroll_state = crate::scroll::ScrollState::with_viewport(Point::new(12.0, 34.0));
    let b = BrowserTabState::new(tab_b, "about:blank".to_string());

    app.push_tab(a, true);
    app.push_tab(b, false);

    let session = BrowserSession::from_app_state(&app);
    assert_eq!(session.active_window_index, 0);
    assert_eq!(session.home_url, about_pages::ABOUT_NEWTAB);
    assert_eq!(session.windows[0].active_tab_index, 0);
    assert!(
      session.windows[0].tab_groups.is_empty(),
      "expected no tab groups in session snapshot"
    );
    assert_eq!(
      session.windows[0].tabs,
      vec![
        BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: Some(1.5),
          scroll_css: Some((12.0, 34.0)),
          pinned: false,
          group: None,
        },
        BrowserSessionTab {
          url: "about:blank".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        },
      ]
    );
    assert!(session.windows[0].tab_groups.is_empty());
    assert_eq!(session.ui_scale, None);
  }

  #[test]
  fn from_app_state_serializes_pinned_and_tab_groups() {
    use crate::ui::{BrowserAppState, BrowserTabState, TabId};

    let mut app = BrowserAppState::new();
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let tab_c = TabId(3);

    app.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab_b, "about:blank".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(tab_c, "about:error".to_string()),
      false,
    );

    assert!(app.pin_tab(tab_a));

    let group = app.create_group_with_tabs(&[tab_b, tab_c]);
    app.set_group_title(group, "My Group".to_string());
    app.set_group_color(group, TabGroupColor::Red);
    // Active tab is the pinned tab (outside the group), so collapsing should stick.
    app.toggle_group_collapsed(group);
    assert!(app.tab_groups.get(&group).is_some_and(|g| g.collapsed));

    let window = BrowserSessionWindow::from_app_state(&app);
    assert_eq!(window.tab_groups.len(), 1);
    assert_eq!(window.tab_groups[0].title, "My Group");
    assert_eq!(window.tab_groups[0].color, TabGroupColor::Red);
    assert!(window.tab_groups[0].collapsed);

    assert_eq!(window.tabs.len(), 3);
    assert!(window.tabs[0].pinned);
    assert_eq!(window.tabs[0].group, None);
    assert_eq!(window.tabs[1].group, Some(0));
    assert_eq!(window.tabs[2].group, Some(0));
  }

  #[test]
  fn session_roundtrips_pinned_tabs_and_tab_groups() {
    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: None,
            pinned: true,
            group: None,
          },
          BrowserSessionTab {
            url: "about:blank".to_string(),
            zoom: Some(1.25),
            scroll_css: None,
            pinned: false,
            group: Some(0),
          },
          BrowserSessionTab {
            url: "about:error".to_string(),
            zoom: None,
            scroll_css: None,
            pinned: false,
            group: Some(1),
          },
        ],
        downloads: Vec::new(),
        tab_groups: vec![
          BrowserSessionTabGroup {
            title: "Work".to_string(),
            color: TabGroupColor::Red,
            collapsed: true,
          },
          BrowserSessionTabGroup {
            title: "Fun".to_string(),
            color: TabGroupColor::Green,
            collapsed: false,
          },
        ],
        closed_tabs: Vec::new(),
        active_tab_index: 2,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    let json = serde_json::to_string(&session).expect("serialize session");
    let parsed = parse_session_json(&json).expect("parse session JSON");
    assert_eq!(parsed, session);
  }

  #[test]
  fn session_roundtrips_downloads() {
    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: vec![
          BrowserSessionDownload {
            url: "https://example.com/a.txt".to_string(),
            file_name: "a.txt".to_string(),
            path: PathBuf::from("/tmp/a.txt"),
            status: BrowserSessionDownloadStatus::Completed,
            error: None,
          },
          BrowserSessionDownload {
            url: "https://example.com/b.txt".to_string(),
            file_name: "b.txt".to_string(),
            path: PathBuf::from("/tmp/b.txt"),
            status: BrowserSessionDownloadStatus::Cancelled,
            error: None,
          },
          BrowserSessionDownload {
            url: "https://example.com/c.txt".to_string(),
            file_name: "c.txt".to_string(),
            path: PathBuf::from("/tmp/c.txt"),
            status: BrowserSessionDownloadStatus::Failed,
            error: Some("network error".to_string()),
          },
        ],
        tab_groups: Vec::new(),
        active_tab_index: 0,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      ui_scale: None,
    }
    .sanitized();

    let json = serde_json::to_string(&session).expect("serialize session");
    let parsed = parse_session_json(&json).expect("parse session JSON");
    assert_eq!(parsed, session);
  }

  #[test]
  fn session_sanitizes_in_progress_downloads_as_cancelled() {
    let window = BrowserSessionWindow {
      tabs: vec![BrowserSessionTab {
        url: "about:newtab".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      }],
      downloads: vec![BrowserSessionDownload {
        url: "https://example.com/a.bin".to_string(),
        file_name: "a.bin".to_string(),
        path: PathBuf::from("/tmp/a.bin"),
        status: BrowserSessionDownloadStatus::InProgress,
        error: None,
      }],
      tab_groups: Vec::new(),
      active_tab_index: 0,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.downloads.len(), 1);
    assert_eq!(window.downloads[0].status, BrowserSessionDownloadStatus::Cancelled);
  }

  #[test]
  fn session_caps_persisted_downloads_and_eviction_is_fifo() {
    let mut downloads = Vec::new();
    for idx in 0..(MAX_PERSISTED_DOWNLOADS + 2) {
      downloads.push(BrowserSessionDownload {
        url: format!("https://example.com/{idx}"),
        file_name: format!("{idx}.bin"),
        path: PathBuf::from(format!("/tmp/{idx}.bin")),
        status: BrowserSessionDownloadStatus::Completed,
        error: None,
      });
    }

    let window = BrowserSessionWindow {
      tabs: vec![BrowserSessionTab {
        url: "about:newtab".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      }],
      downloads,
      tab_groups: Vec::new(),
      active_tab_index: 0,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.downloads.len(), MAX_PERSISTED_DOWNLOADS);
    assert_eq!(
      window.downloads[0].file_name,
      "2.bin",
      "expected oldest downloads to be evicted first"
    );
    assert_eq!(
      window.downloads[MAX_PERSISTED_DOWNLOADS - 1].file_name,
      format!("{}.bin", MAX_PERSISTED_DOWNLOADS + 1)
    );
  }

  #[test]
  fn loads_legacy_v1_session_as_single_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    std::fs::write(
      &path,
      r#"{
        "tabs": [
          {"url": "about:newtab"},
          {"url": "about:blank", "zoom": 1.5}
        ],
        "active_tab_index": 1
      }"#,
    )
    .unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.version, SESSION_VERSION);
    assert_eq!(session.home_url, about_pages::ABOUT_NEWTAB);
    assert_eq!(session.windows.len(), 1);
    assert_eq!(session.active_window_index, 0);
    assert_eq!(session.windows[0].active_tab_index, 1);
    assert!(session.windows[0].tab_groups.is_empty());
    assert_eq!(
      session.windows[0].tabs,
      vec![
        BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        },
        BrowserSessionTab {
          url: "about:blank".to_string(),
          zoom: Some(1.5),
          scroll_css: None,
          pinned: false,
          group: None,
        }
      ]
    );
    assert!(session.windows[0].tab_groups.is_empty());
  }

  #[test]
  fn load_session_rejects_oversized_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let mut file = File::create(&path).unwrap();
    file.set_len(MAX_SESSION_FILE_BYTES + 1).unwrap();
    drop(file);

    let err = load_session(&path).expect_err("expected oversized session file to be rejected");
    assert!(
      err.contains(&MAX_SESSION_FILE_BYTES.to_string()),
      "expected error to mention size limit, got: {err}"
    );
  }

  #[test]
  fn load_session_falls_back_to_backup_when_main_is_oversized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let mut file = File::create(&path).unwrap();
    file.set_len(MAX_SESSION_FILE_BYTES + 1).unwrap();
    drop(file);

    let backup_path = session_backup_path(&path);
    let backup_session = BrowserSession::single("about:blank".to_string());
    std::fs::write(
      &backup_path,
      serde_json::to_string(&backup_session).expect("serialize backup session"),
    )
    .unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session, backup_session);
  }

  #[test]
  fn session_backup_path_appends_bak_suffix_after_existing_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    assert_eq!(session_backup_path(&path), dir.path().join("session.json.bak"));
  }

  #[test]
  fn session_sanitizes_empty_and_invalid_indices() {
    let session = BrowserSession {
      version: 999,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![
        BrowserSessionWindow {
          tabs: vec![],
          downloads: Vec::new(),
          tab_groups: Vec::new(),
          closed_tabs: Vec::new(),
          active_tab_index: 123,
          bookmarks_bar_visible: false,
          show_menu_bar: default_show_menu_bar(),
          window_state: None,
        },
        BrowserSessionWindow {
          tabs: vec![BrowserSessionTab {
            url: "about:blank".to_string(),
            zoom: None,
            scroll_css: None,
            pinned: false,
            group: None,
          }],
          downloads: Vec::new(),
          tab_groups: Vec::new(),
          closed_tabs: Vec::new(),
          active_tab_index: 999,
          bookmarks_bar_visible: false,
          show_menu_bar: default_show_menu_bar(),
          window_state: None,
        },
      ],
      active_window_index: 999,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    assert_eq!(session.version, SESSION_VERSION);
    assert_eq!(session.windows.len(), 2);
    assert_eq!(session.active_window_index, 1);

    assert_eq!(session.windows[0].tabs.len(), 1);
    assert_eq!(session.windows[0].active_tab_index, 0);
    assert_eq!(session.windows[0].tabs[0].url, "about:newtab");

    assert_eq!(session.windows[1].tabs.len(), 1);
    assert_eq!(session.windows[1].active_tab_index, 0);
  }

  #[test]
  fn session_truncates_windows_and_clamps_active_window_index() {
    let mut windows = Vec::new();
    for _ in 0..(MAX_SESSION_WINDOWS + 5) {
      windows.push(BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        tab_groups: Vec::new(),
        active_tab_index: 0,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      });
    }

    let session = BrowserSession {
      version: 999,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows,
      active_window_index: MAX_SESSION_WINDOWS + 123,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    assert_eq!(session.windows.len(), MAX_SESSION_WINDOWS);
    assert_eq!(session.active_window_index, MAX_SESSION_WINDOWS - 1);
  }

  #[test]
  fn window_truncates_tabs_and_clamps_active_tab_index() {
    let mut tabs = Vec::new();
    for _ in 0..(MAX_SESSION_TABS_PER_WINDOW + 10) {
      tabs.push(BrowserSessionTab {
        url: "about:blank".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      });
    }

    let window = BrowserSessionWindow {
      tabs,
      tab_groups: Vec::new(),
      active_tab_index: MAX_SESSION_TABS_PER_WINDOW + 999,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.tabs.len(), MAX_SESSION_TABS_PER_WINDOW);
    assert_eq!(window.active_tab_index, MAX_SESSION_TABS_PER_WINDOW - 1);
  }

  #[test]
  fn window_truncates_tab_groups_and_ungroups_tabs_for_dropped_groups() {
    let mut tab_groups = Vec::new();
    for idx in 0..(MAX_SESSION_TAB_GROUPS_PER_WINDOW + 2) {
      tab_groups.push(BrowserSessionTabGroup {
        title: format!("g{idx}"),
        color: TabGroupColor::Blue,
        collapsed: false,
      });
    }

    let mut tabs = Vec::new();
    for idx in 0..MAX_SESSION_TAB_GROUPS_PER_WINDOW {
      tabs.push(BrowserSessionTab {
        url: "about:blank".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: Some(idx),
      });
    }
    // This references the first group that will be dropped by `truncate`.
    tabs.push(BrowserSessionTab {
      url: "about:newtab".to_string(),
      zoom: None,
      scroll_css: None,
      pinned: false,
      group: Some(MAX_SESSION_TAB_GROUPS_PER_WINDOW),
    });

    let window = BrowserSessionWindow {
      tabs,
      tab_groups,
      active_tab_index: 0,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.tab_groups.len(), MAX_SESSION_TAB_GROUPS_PER_WINDOW);
    assert_eq!(window.tab_groups[0].title, "g0");
    assert_eq!(
      window.tab_groups[MAX_SESSION_TAB_GROUPS_PER_WINDOW - 1].title,
      format!("g{}", MAX_SESSION_TAB_GROUPS_PER_WINDOW - 1)
    );

    assert_eq!(window.tabs.len(), MAX_SESSION_TAB_GROUPS_PER_WINDOW + 1);
    assert_eq!(window.tabs[0].group, Some(0));
    assert_eq!(
      window.tabs[MAX_SESSION_TAB_GROUPS_PER_WINDOW - 1].group,
      Some(MAX_SESSION_TAB_GROUPS_PER_WINDOW - 1)
    );
    assert_eq!(window.tabs.last().unwrap().group, None);
  }

  #[test]
  fn session_truncates_urls_and_revalidates_after_truncation() {
    // Build a URL which is valid in full form, but becomes invalid if truncated to
    // `MAX_SESSION_URL_BYTES` (ends with an incomplete percent-escape).
    let base = "https://example.com/".to_string();
    assert!(
      base.len() + 1 < MAX_SESSION_URL_BYTES,
      "base URL should be shorter than the truncation limit"
    );
    let fill_len = MAX_SESSION_URL_BYTES - base.len() - 1;
    let long_valid = format!("{base}{}%00", "a".repeat(fill_len));
    assert!(
      validate_user_navigation_url_scheme(&long_valid).is_ok(),
      "expected test URL to be valid before truncation"
    );

    let mut window = BrowserSessionWindow {
      tabs: vec![BrowserSessionTab {
        url: long_valid.clone(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: None,
      }],
      tab_groups: Vec::new(),
      active_tab_index: 0,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    };
    window = window.sanitized();
    assert_eq!(window.tabs[0].url, about_pages::ABOUT_NEWTAB);

    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: long_valid,
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        tab_groups: Vec::new(),
        active_tab_index: 0,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();
    assert_eq!(session.home_url, about_pages::ABOUT_NEWTAB);
  }

  #[test]
  fn tab_group_title_is_truncated_safely() {
    // Use a 4-byte emoji so UTF-8 truncation needs to respect char boundaries.
    let long_title = "🔥".repeat(MAX_SESSION_GROUP_TITLE_BYTES);
    let window = BrowserSessionWindow {
      tabs: vec![BrowserSessionTab {
        url: "about:newtab".to_string(),
        zoom: None,
        scroll_css: None,
        pinned: false,
        group: Some(0),
      }],
      tab_groups: vec![BrowserSessionTabGroup {
        title: long_title,
        color: TabGroupColor::Red,
        collapsed: false,
      }],
      active_tab_index: 0,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }
    .sanitized();

    assert_eq!(window.tab_groups.len(), 1);
    assert!(
      window.tab_groups[0].title.as_bytes().len() <= MAX_SESSION_GROUP_TITLE_BYTES,
      "expected title to be truncated to at most {} bytes, got {}",
      MAX_SESSION_GROUP_TITLE_BYTES,
      window.tab_groups[0].title.as_bytes().len()
    );
  }

  #[test]
  fn session_sanitizes_window_geometry() {
    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: Some(BrowserWindowState {
          x: Some(MAX_WINDOW_POS_ABS_PX + 1),
          y: Some(MAX_WINDOW_POS_ABS_PX),
          width: Some(0),
          height: Some(-5),
          maximized: true,
        }),
      }],
      active_window_index: 0,
      appearance: AppearanceSettings::default(),
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();

    let state = session.windows[0].window_state.as_ref().unwrap();
    assert_eq!(state.x, None);
    assert_eq!(state.y, Some(MAX_WINDOW_POS_ABS_PX));
    assert_eq!(state.width, Some(FALLBACK_WINDOW_WIDTH_PX));
    assert_eq!(state.height, Some(FALLBACK_WINDOW_HEIGHT_PX));
    assert!(state.maximized);
  }

  #[test]
  fn save_session_writes_v2_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let session = BrowserSession::single("about:newtab".to_string());
    save_session_atomic(&path, &session).unwrap();

    let data = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(value.get("version").and_then(|v| v.as_u64()), Some(2));
    assert!(value.get("windows").is_some());
    assert!(value.get("active_window_index").is_some());
    // Legacy v1 top-level keys should never be written.
    assert!(value.get("tabs").is_none());
    assert!(value.get("active_tab_index").is_none());
  }

  #[test]
  fn load_session_recovers_from_backup_when_primary_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let backup_path = session_backup_path(&path);

    let session_v1 = BrowserSession::single("about:blank".to_string());
    save_session_atomic(&path, &session_v1).unwrap();

    // Second save should create/update the backup with the previous contents.
    let session_v2 = BrowserSession::single("about:newtab".to_string());
    save_session_atomic(&path, &session_v2).unwrap();

    assert!(
      backup_path.exists(),
      "expected backup session file to exist at {}",
      backup_path.display()
    );

    // Corrupt the primary session file (parse error), leaving a valid backup.
    std::fs::write(&path, "not valid json").unwrap();

    let recovered = load_session(&path).unwrap().unwrap();
    assert_eq!(recovered, session_v1.sanitized());
  }

  #[test]
  fn save_session_atomic_creates_and_updates_backup_on_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    let backup_path = session_backup_path(&path);

    let session_v1 = BrowserSession::single("about:blank".to_string());
    save_session_atomic(&path, &session_v1).unwrap();

    let session_v2 = BrowserSession::single("https://example.com/".to_string());
    save_session_atomic(&path, &session_v2).unwrap();

    let backup_data = std::fs::read_to_string(&backup_path).expect("read backup session");
    let backup_session = parse_session_json(&backup_data).expect("parse backup session");
    assert_eq!(backup_session, session_v1.sanitized());

    let session_v3 = BrowserSession::single("about:newtab".to_string());
    save_session_atomic(&path, &session_v3).unwrap();

    let backup_data = std::fs::read_to_string(&backup_path).expect("read updated backup session");
    let backup_session = parse_session_json(&backup_data).expect("parse updated backup session");
    assert_eq!(backup_session, session_v2.sanitized());
  }

  #[test]
  fn session_roundtrips_non_default_home_url() {
    let mut session = BrowserSession::single("about:newtab".to_string());
    session.home_url = "https://example.com/".to_string();
    let session = session.sanitized();

    let json = serde_json::to_string(&session).expect("serialize session");
    assert!(
      json.contains("\"home_url\""),
      "expected non-default home_url to be serialized, got: {json}"
    );

    let parsed = parse_session_json(&json).expect("parse session JSON");
    assert_eq!(parsed.home_url, session.home_url);
  }

  #[test]
  fn session_roundtrips_appearance_settings() {
    use crate::ui::theme_parsing::BrowserTheme;

    let mut session = BrowserSession::single("about:newtab".to_string());
    session.appearance = AppearanceSettings {
      theme: BrowserTheme::Dark,
      accent_color: Some("#ff00ff".to_string()),
      ui_scale: 1.25,
      high_contrast: true,
      reduced_motion: true,
    };

    let json = serde_json::to_string(&session).expect("serialize session");
    let parsed = parse_session_json(&json).expect("parse session JSON");
    assert_eq!(parsed.appearance, session.appearance.sanitized());
  }

  #[test]
  fn session_sanitizes_appearance_ui_scale() {
    use crate::ui::appearance::{DEFAULT_UI_SCALE, MAX_UI_SCALE, MIN_UI_SCALE};
    use crate::ui::theme_parsing::BrowserTheme;

    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings {
        theme: BrowserTheme::System,
        accent_color: None,
        ui_scale: 999.0,
        high_contrast: false,
        reduced_motion: false,
      },
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();
    assert_eq!(session.appearance.ui_scale, MAX_UI_SCALE);

    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings {
        theme: BrowserTheme::System,
        accent_color: None,
        ui_scale: 0.01,
        high_contrast: false,
        reduced_motion: false,
      },
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();
    assert_eq!(session.appearance.ui_scale, MIN_UI_SCALE);

    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
          pinned: false,
          group: None,
        }],
        downloads: Vec::new(),
        tab_groups: Vec::new(),
        closed_tabs: Vec::new(),
        active_tab_index: 0,
        bookmarks_bar_visible: false,
        show_menu_bar: default_show_menu_bar(),
        window_state: None,
      }],
      active_window_index: 0,
      appearance: AppearanceSettings {
        theme: BrowserTheme::System,
        accent_color: None,
        ui_scale: f32::NAN,
        high_contrast: false,
        reduced_motion: false,
      },
      did_exit_cleanly: true,
      unclean_exit_streak: 0,
      ui_scale: None,
    }
    .sanitized();
    assert_eq!(session.appearance.ui_scale, DEFAULT_UI_SCALE);
  }

  #[test]
  fn session_parses_unknown_theme_value_as_system() {
    use crate::ui::theme_parsing::BrowserTheme;

    let raw = r#"{
      "version": 2,
      "windows": [{"tabs": [{"url": "about:newtab"}], "active_tab_index": 0}],
      "active_window_index": 0,
      "appearance": {"theme": "wat"}
    }"#;

    let session = parse_session_json(raw).expect("parse session");
    assert_eq!(session.appearance.theme, BrowserTheme::System);
  }

  #[test]
  fn session_lock_fails_when_already_held() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.json");
    let lock_path = session_path.with_extension("lock");

    let lock = acquire_session_lock(&session_path).expect("acquire first lock");
    assert!(
      lock_path.exists(),
      "expected lock file to exist at {}",
      lock_path.display()
    );

    let err = acquire_session_lock(&session_path).expect_err("second lock should fail");
    assert!(
      matches!(err, SessionLockError::AlreadyLocked { .. }),
      "expected AlreadyLocked error, got {err:?}"
    );

    drop(lock);
    acquire_session_lock(&session_path).expect("lock should be acquirable after drop");
  }

  #[test]
  fn load_session_falls_back_to_backup_when_primary_is_corrupted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.json");
    let backup_path = session_backup_path(&session_path);

    std::fs::write(&session_path, "{not valid json").expect("write corrupted primary session");

    let backup_json = r#"{
      "version": 2,
      "home_url": "about:blank",
      "windows": [{
        "tabs": [
          {"url": "about:blank", "zoom": 1.25},
          {"url": "about:error", "zoom": 0.75, "pinned": true}
        ],
        "active_tab_index": 1
      }],
      "active_window_index": 0
    }"#;
    std::fs::write(&backup_path, backup_json).expect("write backup session");

    let expected = parse_session_json(backup_json).expect("parse expected backup session JSON");
    let loaded = load_session(&session_path)
      .expect("load session")
      .expect("expected session");
    assert_eq!(loaded, expected);
  }
}

/// Determine the on-disk session file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_SESSION_PATH` env var (used by integration tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_session.json` in the current working directory.
pub fn session_path() -> PathBuf {
  if let Some(raw) = std::env::var_os(SESSION_ENV_PATH) {
    if !raw.is_empty() {
      return PathBuf::from(raw);
    }
  }

  if let Some(base_dirs) = directories::BaseDirs::new() {
    return base_dirs
      .config_dir()
      .join("fastrender")
      .join(SESSION_FILE_NAME);
  }

PathBuf::from(format!("./{SESSION_FILE_NAME}"))
}

fn session_backup_path(path: &Path) -> PathBuf {
  let Some(file_name) = path.file_name() else {
    return path.with_extension("bak");
  };
  let mut backup_name = file_name.to_os_string();
  backup_name.push(".bak");
  path.with_file_name(backup_name)
}

fn read_session_file_bounded(path: &Path) -> Result<Option<String>, String> {
  let file = match File::open(path) {
    Ok(file) => file,
    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(format!("failed to open {}: {err}", path.display())),
  };

  let size = file
    .metadata()
    .map_err(|err| format!("failed to stat {}: {err}", path.display()))?
    .len();
  if size > MAX_SESSION_FILE_BYTES {
    return Err(format!(
      "refusing to load session {}: file is {size} bytes, exceeding the maximum supported size of {MAX_SESSION_FILE_BYTES} bytes ({} MiB)",
      path.display(),
      MAX_SESSION_FILE_BYTES / (1024 * 1024)
    ));
  }

  let mut limited = file.take(MAX_SESSION_FILE_BYTES.saturating_add(1));
  let mut buf = Vec::with_capacity(size as usize);
  limited
    .read_to_end(&mut buf)
    .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
  if (buf.len() as u64) > MAX_SESSION_FILE_BYTES {
    // Defensive check in case the file changed size after the metadata check (or for non-regular
    // files where `metadata.len()` is unreliable).
    return Err(format!(
      "refusing to load session {}: file exceeds the maximum supported size of {MAX_SESSION_FILE_BYTES} bytes ({} MiB)",
      path.display(),
      MAX_SESSION_FILE_BYTES / (1024 * 1024)
    ));
  }

  String::from_utf8(buf)
    .map(Some)
    .map_err(|err| format!("failed to decode {} as UTF-8: {err}", path.display()))
}
/// Attempt to read + parse a session file. Missing file is not an error.
///
/// If the primary session file is missing, unreadable, corrupt, or too large, we will attempt to
/// fall back to a `.bak` backup session file in the same directory.
pub fn load_session(path: &Path) -> Result<Option<BrowserSession>, String> {
  let data = match read_session_file_bounded(path) {
    Ok(Some(data)) => data,
    Ok(None) => return Ok(None),
    Err(err) => {
      let backup = session_backup_path(path);
      match read_session_file_bounded(&backup) {
        Ok(Some(data)) => {
          let session = parse_session_json(&data).map_err(|backup_err| {
            format!(
              "{err}; also failed to parse backup {}: {backup_err}",
              backup.display()
            )
          })?;
          eprintln!(
            "failed to load session file {} ({err}); recovered from backup {}",
            path.display(),
            backup.display()
          );
          return Ok(Some(session));
        }
        Ok(None) => return Err(err),
        Err(backup_err) => {
          return Err(format!(
            "{err}; also failed to read backup {}: {backup_err}",
            backup.display()
          ))
        }
      }
    }
  };

  match parse_session_json(&data) {
    Ok(session) => Ok(Some(session)),
    Err(primary_err) => {
      let backup_path = session_backup_path(path);
      let backup_data = match read_session_file_bounded(&backup_path) {
        Ok(Some(data)) => data,
        Ok(None) => return Err(format!("failed to parse {}: {primary_err}", path.display())),
        Err(err) => {
          return Err(format!(
            "failed to parse {}: {primary_err}; also failed to read backup {}: {err}",
            path.display(),
            backup_path.display()
          ));
        }
      };

      match parse_session_json(&backup_data) {
        Ok(session) => {
          eprintln!(
            "session file {} was unreadable ({primary_err}); recovered from backup {}",
            path.display(),
            backup_path.display()
          );
          Ok(Some(session))
        }
        Err(backup_err) => Err(format!(
          "failed to parse {}: {primary_err}; also failed to parse backup {}: {backup_err}",
          path.display(),
          backup_path.display()
        )),
      }
    }
  }
}

/// Write the session file atomically (write temp file + rename).
pub fn save_session_atomic(path: &Path, session: &BrowserSession) -> Result<(), String> {
  let session = session.clone().sanitized();

  let parent_dir = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  std::fs::create_dir_all(parent_dir)
    .map_err(|err| format!("failed to create {}: {err}", parent_dir.display()))?;

  // Best-effort safety net: preserve the existing session (when it parses successfully) as a backup
  // before overwriting it.
  //
  // This allows recovery from disk corruption or manual edits that make `session.json` unreadable.
  // We intentionally do *not* fail the save if updating the backup fails.
  if let Ok(Some(existing)) = read_session_file_bounded(path) {
    if parse_session_json(&existing).is_ok() {
      let backup_path = session_backup_path(path);
      let _ = std::fs::write(&backup_path, existing);
    }
  }

  let data = serde_json::to_vec_pretty(&session).map_err(|err| err.to_string())?;

  let mut tmp = tempfile::NamedTempFile::new_in(parent_dir).map_err(|err| {
    format!(
      "failed to create temp session file in {}: {err}",
      parent_dir.display()
    )
  })?;
  use std::io::Write;
  tmp
    .write_all(&data)
    .map_err(|err| format!("failed to write temp session file: {err}"))?;
  tmp
    .flush()
    .map_err(|err| format!("failed to flush temp session file: {err}"))?;

  // Best-effort durability: don't fail the whole save if syncing is unsupported.
  let _ = tmp.as_file().sync_all();

  match tmp.persist(path) {
    Ok(_) => Ok(()),
    Err(err) => {
      // On Windows, rename fails if the destination exists. Fall back to removing the existing file
      // and retrying (not strictly atomic, but best-effort cross-platform).
      if matches!(
        err.error.kind(),
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
      ) {
        let _ = std::fs::remove_file(path);
        err.file.persist(path).map(|_| ()).map_err(|err| {
          format!(
            "failed to persist session file {}: {}",
            path.display(),
            err.error
          )
        })
      } else {
        Err(format!(
          "failed to persist session file {}: {}",
          path.display(),
          err.error
        ))
      }
    }
  }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct BrowserSessionV1 {
  #[serde(default)]
  tabs: Vec<BrowserSessionTab>,
  #[serde(default)]
  active_tab_index: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
enum BrowserSessionFile {
  V2(BrowserSession),
  V1(BrowserSessionV1),
}

fn v1_into_v2(v1: BrowserSessionV1) -> BrowserSession {
  BrowserSession {
    version: SESSION_VERSION,
    home_url: default_home_url(),
    windows: vec![BrowserSessionWindow {
      tabs: v1.tabs,
      downloads: Vec::new(),
      tab_groups: Vec::new(),
      closed_tabs: Vec::new(),
      active_tab_index: v1.active_tab_index,
      bookmarks_bar_visible: false,
      show_menu_bar: default_show_menu_bar(),
      window_state: None,
    }],
    active_window_index: 0,
    appearance: AppearanceSettings::default(),
    did_exit_cleanly: true,
    unclean_exit_streak: 0,
    ui_scale: None,
  }
}

/// Parse a session JSON payload (v2 or legacy v1) into the in-memory [`BrowserSession`] model.
pub fn parse_session_json(raw: &str) -> Result<BrowserSession, String> {
  let parsed: BrowserSessionFile = serde_json::from_str(raw).map_err(|err| err.to_string())?;
  let session = match parsed {
    BrowserSessionFile::V2(session) => session,
    BrowserSessionFile::V1(v1) => v1_into_v2(v1),
  };
  Ok(session.sanitized())
}

/// A best-effort advisory file lock used to ensure only one `browser` process writes a session file.
///
/// This prevents multiple instances from racing and clobbering each other's autosaved session state.
#[derive(Debug)]
pub struct SessionFileLock {
  _file: File,
  path: PathBuf,
}

impl SessionFileLock {
  pub fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for SessionFileLock {
  fn drop(&mut self) {
    let _ = self._file.unlock();
  }
}

#[derive(Debug)]
pub enum SessionLockError {
  AlreadyLocked {
    lock_path: PathBuf,
  },
  Io {
    lock_path: PathBuf,
    error: io::Error,
  },
}

impl std::fmt::Display for SessionLockError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::AlreadyLocked { lock_path } => {
        write!(f, "session lock already held: {}", lock_path.display())
      }
      Self::Io { lock_path, error } => {
        write!(
          f,
          "failed to acquire session lock {}: {error}",
          lock_path.display()
        )
      }
    }
  }
}

impl std::error::Error for SessionLockError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::AlreadyLocked { .. } => None,
      Self::Io { error, .. } => Some(error),
    }
  }
}

/// Acquire an exclusive advisory lock for a session file.
///
/// The lock is held for as long as the returned [`SessionFileLock`] value is kept alive.
pub fn acquire_session_lock(session_path: &Path) -> Result<SessionFileLock, SessionLockError> {
  let lock_path = session_path.with_extension("lock");

  let parent_dir = session_path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  std::fs::create_dir_all(parent_dir).map_err(|error| SessionLockError::Io {
    lock_path: lock_path.clone(),
    error,
  })?;

  let file = match OpenOptions::new()
    .create(true)
    .read(true)
    .write(true)
    .open(&lock_path)
  {
    Ok(file) => file,
    Err(error) => return Err(SessionLockError::Io { lock_path, error }),
  };

  match file.try_lock_exclusive() {
    Ok(()) => Ok(SessionFileLock {
      _file: file,
      path: lock_path,
    }),
    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
      Err(SessionLockError::AlreadyLocked { lock_path })
    }
    Err(error) => Err(SessionLockError::Io { lock_path, error }),
  }
}
