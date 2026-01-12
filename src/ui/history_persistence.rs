//! Browser UI global history persistence.
//!
//! This stores an opaque JSON snapshot for the browser's "global history" state (distinct from the
//! per-tab back/forward list in `ui::history`). Keeping this opaque allows the UI to evolve without
//! committing to a stable schema up front.

use std::path::{Path, PathBuf};

const HISTORY_ENV_PATH: &str = "FASTR_BROWSER_HISTORY_PATH";
const HISTORY_FILE_NAME: &str = "fastrender_history.json";

/// Opaque global history snapshot persisted to disk.
pub type HistorySnapshot = serde_json::Value;

/// Determine the on-disk history file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_HISTORY_PATH` env var (used by integration tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_history.json` in the current working directory.
pub fn history_path() -> PathBuf {
  if let Some(raw) = std::env::var_os(HISTORY_ENV_PATH) {
    if !raw.is_empty() {
      return PathBuf::from(raw);
    }
  }

  if let Some(base_dirs) = directories::BaseDirs::new() {
    return base_dirs
      .config_dir()
      .join("fastrender")
      .join(HISTORY_FILE_NAME);
  }

  PathBuf::from(format!("./{HISTORY_FILE_NAME}"))
}

/// Attempt to read + parse a history file. Missing file is not an error.
pub fn load_history(path: &Path) -> Result<Option<HistorySnapshot>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let history: HistorySnapshot = serde_json::from_str(&data)
    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(Some(history))
}

/// Write the history file atomically (write temp file + rename).
pub fn save_history_atomic(path: &Path, history: &HistorySnapshot) -> Result<(), String> {
  let parent_dir = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  std::fs::create_dir_all(parent_dir)
    .map_err(|err| format!("failed to create {}: {err}", parent_dir.display()))?;

  let data = serde_json::to_vec_pretty(history).map_err(|err| err.to_string())?;

  let mut tmp = tempfile::NamedTempFile::new_in(parent_dir).map_err(|err| {
    format!(
      "failed to create temp history file in {}: {err}",
      parent_dir.display()
    )
  })?;
  use std::io::Write;
  tmp
    .write_all(&data)
    .map_err(|err| format!("failed to write temp history file: {err}"))?;
  tmp
    .flush()
    .map_err(|err| format!("failed to flush temp history file: {err}"))?;

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
            "failed to persist history file {}: {}",
            path.display(),
            err.error
          )
        })
      } else {
        Err(format!(
          "failed to persist history file {}: {}",
          path.display(),
          err.error
        ))
      }
    }
  }
}

