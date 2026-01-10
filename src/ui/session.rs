//! Browser UI session persistence (tabs + active tab).
//!
//! Kept behind the `browser_ui` feature gate so core renderer builds remain lean.

use crate::ui::about_pages;
use crate::ui::browser_app::BrowserAppState;
use crate::ui::validate_user_navigation_url_scheme;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SESSION_ENV_PATH: &str = "FASTR_BROWSER_SESSION_PATH";
const SESSION_FILE_NAME: &str = "fastrender_session.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSessionTab {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub zoom: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserSession {
  pub tabs: Vec<BrowserSessionTab>,
  pub active_tab_index: usize,
}

impl BrowserSession {
  pub fn single(url: String) -> Self {
    Self {
      tabs: vec![BrowserSessionTab { url, zoom: None }],
      active_tab_index: 0,
    }
    .sanitized()
  }

  /// Build a session snapshot from the current windowed UI state model.
  ///
  /// This intentionally stores only lightweight serializable data (URLs, zoom).
  pub fn from_app_state(app: &BrowserAppState) -> Self {
    let mut tabs = Vec::new();
    for tab in &app.tabs {
      let mut url = tab
        .current_url
        .clone()
        .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
      if validate_user_navigation_url_scheme(&url).is_err() {
        url = about_pages::ABOUT_NEWTAB.to_string();
      }
      tabs.push(BrowserSessionTab { url, zoom: None });
    }

    let active_tab_index = app
      .active_tab_id()
      .and_then(|id| app.tabs.iter().position(|t| t.id == id))
      .unwrap_or(0);

    Self {
      tabs,
      active_tab_index,
    }
    .sanitized()
  }

  /// Ensure the session is well-formed and contains only supported URLs.
  pub fn sanitized(mut self) -> Self {
    if self.tabs.is_empty() {
      self.tabs.push(BrowserSessionTab {
        url: about_pages::ABOUT_NEWTAB.to_string(),
        zoom: None,
      });
      self.active_tab_index = 0;
    }

    for tab in &mut self.tabs {
      if tab.url.trim().is_empty() || validate_user_navigation_url_scheme(&tab.url).is_err() {
        tab.url = about_pages::ABOUT_NEWTAB.to_string();
      }
    }

    if self.active_tab_index >= self.tabs.len() {
      self.active_tab_index = 0;
    }

    self
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

  let session: BrowserSession = serde_json::from_str(&data)
    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(Some(session.sanitized()))
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

  let mut tmp = tempfile::NamedTempFile::new_in(parent_dir)
    .map_err(|err| format!("failed to create temp session file in {}: {err}", parent_dir.display()))?;
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
        err
          .file
          .persist(path)
          .map(|_| ())
          .map_err(|err| format!("failed to persist session file {}: {}", path.display(), err.error))
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
