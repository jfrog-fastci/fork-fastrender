//! Browser UI session persistence (windows + tabs + active selection).
//!
//! Kept behind the `browser_ui` feature gate so core renderer builds remain lean.

use crate::ui::about_pages;
use crate::ui::browser_app::BrowserAppState;
use crate::ui::theme;
use crate::ui::validate_user_navigation_url_scheme;
use crate::ui::zoom;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

const SESSION_ENV_PATH: &str = "FASTR_BROWSER_SESSION_PATH";
const SESSION_FILE_NAME: &str = "fastrender_session.json";
const SESSION_VERSION: u32 = 2;

const MAX_WINDOW_DIM_PX: i64 = 16_384;
const MAX_WINDOW_POS_ABS_PX: i64 = 1_000_000;
const FALLBACK_WINDOW_WIDTH_PX: i64 = 1_024;
const FALLBACK_WINDOW_HEIGHT_PX: i64 = 768;
const MAX_SCROLL_CSS: f32 = 1e9;

fn default_did_exit_cleanly() -> bool {
  true
}

fn is_true(value: &bool) -> bool {
  *value
}

fn is_false(value: &bool) -> bool {
  !*value
}

fn default_home_url() -> String {
  about_pages::ABOUT_NEWTAB.to_string()
}

fn is_default_home_url(url: &String) -> bool {
  url == about_pages::ABOUT_NEWTAB
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
  #[serde(default)]
  pub active_tab_index: usize,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub window_state: Option<BrowserWindowState>,
}

impl BrowserSessionWindow {
  /// Ensure the window is well-formed and contains only supported URLs.
  pub fn sanitized(mut self) -> Self {
    if self.tabs.is_empty() {
      self.tabs.push(BrowserSessionTab {
        url: about_pages::ABOUT_NEWTAB.to_string(),
        zoom: None,
        scroll_css: None,
      });
      self.active_tab_index = 0;
    }

    for tab in &mut self.tabs {
      sanitize_tab(tab);
    }

    self.active_tab_index = self
      .active_tab_index
      .min(self.tabs.len().saturating_sub(1));

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
        }],
        active_tab_index: 0,
        window_state: None,
      }],
      active_window_index: 0,
      did_exit_cleanly: true,
      ui_scale: None,
    }
    .sanitized()
  }

  /// Build a session snapshot from the current windowed UI state model.
  ///
  /// This intentionally stores only lightweight serializable data (URLs, zoom, viewport scroll).
  pub fn from_app_state(app: &BrowserAppState, ui_scale: f32) -> Self {
    let ui_scale = theme::clamp_ui_scale(ui_scale);
    let mut tabs = Vec::new();
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
      tabs.push(BrowserSessionTab {
        url,
        zoom: Some(tab.zoom),
        scroll_css,
      });
    }

    let active_tab_index = app
      .active_tab_id()
      .and_then(|id| app.tabs.iter().position(|t| t.id == id))
      .unwrap_or(0);

    Self {
      version: SESSION_VERSION,
      home_url: default_home_url(),
      windows: vec![BrowserSessionWindow {
        tabs,
        active_tab_index,
        window_state: None,
      }],
      active_window_index: 0,
      did_exit_cleanly: true,
      ui_scale: (ui_scale != theme::DEFAULT_UI_SCALE).then_some(ui_scale),
    }
    .sanitized()
  }

  /// Ensure the session is well-formed and contains only supported URLs.
  pub fn sanitized(mut self) -> Self {
    self.version = SESSION_VERSION;

    let home_trimmed = self.home_url.trim().to_string();
    self.home_url = if home_trimmed.is_empty()
      || validate_user_navigation_url_scheme(&home_trimmed).is_err()
    {
      default_home_url()
    } else {
      home_trimmed
    };

    if self.windows.is_empty() {
      self.windows.push(BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: about_pages::ABOUT_NEWTAB.to_string(),
          zoom: None,
          scroll_css: None,
        }],
        active_tab_index: 0,
        window_state: None,
      });
      self.active_window_index = 0;
    }

    self.windows = std::mem::take(&mut self.windows)
      .into_iter()
      .map(|window| window.sanitized())
      .collect();

    self.active_window_index = self
      .active_window_index
      .min(self.windows.len().saturating_sub(1));

    self.ui_scale = self
      .ui_scale
      .map(|raw| theme::clamp_ui_scale(raw))
      .and_then(|scale| (scale != theme::DEFAULT_UI_SCALE).then_some(scale));

    self
  }
}

fn sanitize_tab(tab: &mut BrowserSessionTab) {
  if tab.url.trim().is_empty() || validate_user_navigation_url_scheme(&tab.url).is_err() {
    tab.url = about_pages::ABOUT_NEWTAB.to_string();
  }

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
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(f32::INFINITY),
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(0.0),
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(-1.0),
            scroll_css: None,
          },
          // Finite but outside the supported UI range should clamp.
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(0.1),
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(999.0),
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(2.0),
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: Some(zoom::DEFAULT_ZOOM),
            scroll_css: None,
          },
        ],
        active_tab_index: 0,
        window_state: None,
      }],
      active_window_index: 0,
      did_exit_cleanly: true,
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
  fn session_omits_default_scroll_from_json() {
    let session = BrowserSession::single("about:newtab".to_string());
    let json = serde_json::to_string(&session).expect("serialize session");
    assert!(
      !json.contains("scroll_css"),
      "expected default scroll to be omitted from JSON, got: {json}"
    );
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
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((1.0, f32::INFINITY)),
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((-5.0, -3.0)),
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((-5.0, 25.0)),
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((1e12, 5.0)),
          },
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: Some((0.0, 0.0)),
          },
        ],
        active_tab_index: 0,
        window_state: None,
      }],
      active_window_index: 0,
      did_exit_cleanly: true,
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

    let session = BrowserSession::from_app_state(&app, theme::DEFAULT_UI_SCALE);
    assert_eq!(session.active_window_index, 0);
    assert_eq!(session.home_url, about_pages::ABOUT_NEWTAB);
    assert_eq!(session.windows[0].active_tab_index, 0);
    assert_eq!(
      session.windows[0].tabs,
      vec![
        BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: Some(1.5),
          scroll_css: Some((12.0, 34.0)),
        },
        BrowserSessionTab {
          url: "about:blank".to_string(),
          zoom: None,
          scroll_css: None,
        },
      ]
    );
    assert_eq!(session.ui_scale, None);
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
      assert_eq!(
        session.windows[0].tabs,
        vec![
          BrowserSessionTab {
            url: "about:newtab".to_string(),
            zoom: None,
            scroll_css: None,
          },
          BrowserSessionTab {
            url: "about:blank".to_string(),
            zoom: Some(1.5),
            scroll_css: None,
          }
        ]
      );
  }

  #[test]
  fn session_sanitizes_empty_and_invalid_indices() {
    let session = BrowserSession {
      version: 999,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![
        BrowserSessionWindow {
          tabs: vec![],
          active_tab_index: 123,
          window_state: None,
        },
        BrowserSessionWindow {
          tabs: vec![BrowserSessionTab {
            url: "about:blank".to_string(),
            zoom: None,
            scroll_css: None,
          }],
          active_tab_index: 999,
          window_state: None,
        },
      ],
      active_window_index: 999,
      did_exit_cleanly: true,
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
  fn session_sanitizes_window_geometry() {
    let session = BrowserSession {
      version: SESSION_VERSION,
      home_url: about_pages::ABOUT_NEWTAB.to_string(),
      windows: vec![BrowserSessionWindow {
        tabs: vec![BrowserSessionTab {
          url: "about:newtab".to_string(),
          zoom: None,
          scroll_css: None,
        }],
        active_tab_index: 0,
        window_state: Some(BrowserWindowState {
          x: Some(MAX_WINDOW_POS_ABS_PX + 1),
          y: Some(MAX_WINDOW_POS_ABS_PX),
          width: Some(0),
          height: Some(-5),
          maximized: true,
        }),
      }],
      active_window_index: 0,
      did_exit_cleanly: true,
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

/// Attempt to read + parse a session file. Missing file is not an error.
pub fn load_session(path: &Path) -> Result<Option<BrowserSession>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let session = parse_session_json(&data)
    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(Some(session))
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
      active_tab_index: v1.active_tab_index,
      window_state: None,
    }],
    active_window_index: 0,
    did_exit_cleanly: true,
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
  AlreadyLocked { lock_path: PathBuf },
  Io { lock_path: PathBuf, error: io::Error },
}

impl std::fmt::Display for SessionLockError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::AlreadyLocked { lock_path } => {
        write!(f, "session lock already held: {}", lock_path.display())
      }
      Self::Io { lock_path, error } => {
        write!(f, "failed to acquire session lock {}: {error}", lock_path.display())
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
