//! Browser UI bookmarks persistence.
//!
//! This module is intentionally lightweight: it stores an opaque JSON snapshot so the UI can
//! iterate on bookmark schema without rewriting migration logic up-front. The headless smoke mode
//! uses this to exercise persistence in CI.

use std::path::{Path, PathBuf};

const BOOKMARKS_ENV_PATH: &str = "FASTR_BROWSER_BOOKMARKS_PATH";
const BOOKMARKS_FILE_NAME: &str = "fastrender_bookmarks.json";

/// Opaque bookmarks snapshot persisted to disk.
pub type BookmarksSnapshot = serde_json::Value;

/// Determine the on-disk bookmarks file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_BOOKMARKS_PATH` env var (used by integration tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_bookmarks.json` in the current working directory.
pub fn bookmarks_path() -> PathBuf {
  if let Some(raw) = std::env::var_os(BOOKMARKS_ENV_PATH) {
    if !raw.is_empty() {
      return PathBuf::from(raw);
    }
  }

  if let Some(base_dirs) = directories::BaseDirs::new() {
    return base_dirs
      .config_dir()
      .join("fastrender")
      .join(BOOKMARKS_FILE_NAME);
  }

  PathBuf::from(format!("./{BOOKMARKS_FILE_NAME}"))
}

/// Attempt to read + parse a bookmarks file. Missing file is not an error.
pub fn load_bookmarks(path: &Path) -> Result<Option<BookmarksSnapshot>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let bookmarks: BookmarksSnapshot = serde_json::from_str(&data)
    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(Some(bookmarks))
}

/// Write the bookmarks file atomically (write temp file + rename).
pub fn save_bookmarks_atomic(path: &Path, bookmarks: &BookmarksSnapshot) -> Result<(), String> {
  let parent_dir = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  std::fs::create_dir_all(parent_dir)
    .map_err(|err| format!("failed to create {}: {err}", parent_dir.display()))?;

  let data = serde_json::to_vec_pretty(bookmarks).map_err(|err| err.to_string())?;

  let mut tmp = tempfile::NamedTempFile::new_in(parent_dir).map_err(|err| {
    format!(
      "failed to create temp bookmarks file in {}: {err}",
      parent_dir.display()
    )
  })?;
  use std::io::Write;
  tmp
    .write_all(&data)
    .map_err(|err| format!("failed to write temp bookmarks file: {err}"))?;
  tmp
    .flush()
    .map_err(|err| format!("failed to flush temp bookmarks file: {err}"))?;

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
            "failed to persist bookmarks file {}: {}",
            path.display(),
            err.error
          )
        })
      } else {
        Err(format!(
          "failed to persist bookmarks file {}: {}",
          path.display(),
          err.error
        ))
      }
    }
  }
}

