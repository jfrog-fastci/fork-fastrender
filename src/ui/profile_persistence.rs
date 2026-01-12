//! Browser UI bookmarks + history persistence.
//!
//! This module is the single authoritative implementation for:
//! - Determining persistence paths (`*_path` helpers)
//! - Loading/saving bookmarks/history JSON
//! - On-disk schema versioning + migrations
//!
//! Both the windowed UI and `browser --headless-smoke` must use these helpers so the schema stays
//! in lockstep.

use crate::ui::bookmarks::BookmarkStore;
use crate::ui::global_history::{GlobalHistoryEntry, GlobalHistoryStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const BOOKMARKS_ENV_PATH: &str = "FASTR_BROWSER_BOOKMARKS_PATH";
const HISTORY_ENV_PATH: &str = "FASTR_BROWSER_HISTORY_PATH";

const BOOKMARKS_FILE_NAME: &str = "fastrender_bookmarks.json";
const HISTORY_FILE_NAME: &str = "fastrender_history.json";

const HISTORY_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSource {
  Disk,
  Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome<T> {
  pub source: LoadSource,
  pub value: T,
}

/// Versioned on-disk global-history schema.
///
/// The in-memory UI model is [`GlobalHistoryStore`] (which is intentionally not versioned); this
/// wrapper is the authoritative persisted schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedGlobalHistoryStore {
  pub version: u32,
  #[serde(default)]
  pub entries: Vec<GlobalHistoryEntry>,
}

impl Default for PersistedGlobalHistoryStore {
  fn default() -> Self {
    Self {
      version: HISTORY_VERSION,
      entries: Vec::new(),
    }
  }
}

impl PersistedGlobalHistoryStore {
  fn sanitized(mut self) -> Self {
    self.version = HISTORY_VERSION;
    self.entries.retain(|e| !e.url.trim().is_empty());
    self
  }

  pub fn from_store(store: &GlobalHistoryStore) -> Self {
    let mut store = store.clone();
    store.normalize_in_place();
    Self {
      version: HISTORY_VERSION,
      entries: store.entries,
    }
    .sanitized()
  }

  pub fn into_store(self) -> GlobalHistoryStore {
    let mut store = GlobalHistoryStore { entries: self.entries };
    store.normalize_in_place();
    store
  }
}

fn profile_path(env_key: &str, file_name: &str) -> PathBuf {
  if let Some(raw) = std::env::var_os(env_key) {
    if !raw.is_empty() {
      return PathBuf::from(raw);
    }
  }

  #[cfg(feature = "browser_ui")]
  {
    if let Some(base_dirs) = directories::BaseDirs::new() {
      return base_dirs.config_dir().join("fastrender").join(file_name);
    }
  }

  PathBuf::from(format!("./{file_name}"))
}

/// Determine the on-disk bookmarks file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_BOOKMARKS_PATH` env var (used by tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_bookmarks.json` in the current working directory.
pub fn bookmarks_path() -> PathBuf {
  profile_path(BOOKMARKS_ENV_PATH, BOOKMARKS_FILE_NAME)
}

/// Determine the on-disk global history file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_HISTORY_PATH` env var (used by tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_history.json` in the current working directory.
pub fn history_path() -> PathBuf {
  profile_path(HISTORY_ENV_PATH, HISTORY_FILE_NAME)
}

/// Attempt to read + parse a bookmarks file.
///
/// Missing files return [`LoadSource::Empty`] rather than an error.
pub fn load_bookmarks(path: &Path) -> Result<LoadOutcome<BookmarkStore>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      return Ok(LoadOutcome {
        source: LoadSource::Empty,
        value: BookmarkStore::default(),
      })
    }
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let store = parse_bookmarks_json(&data)
    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(LoadOutcome {
    source: LoadSource::Disk,
    value: store,
  })
}

/// Attempt to read + parse a history file.
///
/// Missing files return [`LoadSource::Empty`] rather than an error.
pub fn load_history(path: &Path) -> Result<LoadOutcome<GlobalHistoryStore>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      return Ok(LoadOutcome {
        source: LoadSource::Empty,
        value: GlobalHistoryStore::default(),
      })
    }
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let store =
    parse_history_json(&data).map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(LoadOutcome {
    source: LoadSource::Disk,
    value: store,
  })
}

/// Parse a bookmarks JSON payload (canonical `BookmarkStore` or legacy schemas) into the in-memory
/// [`BookmarkStore`] model.
pub fn parse_bookmarks_json(raw: &str) -> Result<BookmarkStore, String> {
  let (store, _migration) = BookmarkStore::from_json_str_migrating(raw).map_err(|err| format!("{err:?}"))?;
  Ok(store)
}

/// Parse a history JSON payload (v1 or legacy schemas) into the in-memory [`GlobalHistoryStore`] model.
pub fn parse_history_json(raw: &str) -> Result<GlobalHistoryStore, String> {
  #[derive(Debug, Clone, Deserialize)]
  struct HeadlessHistoryEntry {
    url: String,
    #[serde(default)]
    title: Option<String>,
    /// Unix epoch milliseconds.
    #[serde(default, alias = "visited_at_ms")]
    ts: Option<u64>,
    #[serde(default)]
    visit_count: Option<u64>,
  }

  #[derive(Debug, Clone, Deserialize)]
  #[serde(untagged)]
  enum HistoryFile {
    V1(PersistedGlobalHistoryStore),
    // Legacy windowed-ui schema: `{ "entries": [...] }` (no version field).
    V0(GlobalHistoryStore),
    // Legacy headless-smoke schema: `[{"title":"...", "url":"...", "ts":123}]`
    HeadlessV0(Vec<HeadlessHistoryEntry>),
  }

  let parsed: HistoryFile = serde_json::from_str(raw).map_err(|err| err.to_string())?;
  Ok(match parsed {
    HistoryFile::V1(store) => {
      if store.version != HISTORY_VERSION {
        return Err(format!(
          "unsupported history version {}; expected {}",
          store.version, HISTORY_VERSION
        ));
      }
      store.into_store()
    }
    HistoryFile::V0(mut store) => {
      store.normalize_in_place();
      store
    }
    HistoryFile::HeadlessV0(entries) => {
      let mut store = GlobalHistoryStore {
        entries: entries
          .into_iter()
          .map(|e| GlobalHistoryEntry {
            url: e.url,
            title: e.title,
            visited_at_ms: e.ts,
            visit_count: e.visit_count.unwrap_or(1).max(1),
          })
          .collect(),
      };
      store.normalize_in_place();
      store
    }
  })
}

/// Write the bookmarks file atomically (write temp file + rename).
pub fn save_bookmarks_atomic(path: &Path, bookmarks: &BookmarkStore) -> Result<(), String> {
  save_json_atomic(path, bookmarks)
}

/// Write the history file atomically (write temp file + rename).
pub fn save_history_atomic(path: &Path, history: &GlobalHistoryStore) -> Result<(), String> {
  let persisted = PersistedGlobalHistoryStore::from_store(history);
  save_json_atomic(path, &persisted)
}

fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
  let parent_dir = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or_else(|| Path::new("."));
  std::fs::create_dir_all(parent_dir)
    .map_err(|err| format!("failed to create {}: {err}", parent_dir.display()))?;

  let data = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;

  let mut tmp = tempfile::NamedTempFile::new_in(parent_dir)
    .map_err(|err| format!("failed to create temp file in {}: {err}", parent_dir.display()))?;
  use std::io::Write;
  tmp
    .write_all(&data)
    .map_err(|err| format!("failed to write temp file: {err}"))?;
  tmp
    .flush()
    .map_err(|err| format!("failed to flush temp file: {err}"))?;

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
          .map_err(|err| format!("failed to persist {}: {}", path.display(), err.error))
      } else {
        Err(format!("failed to persist {}: {}", path.display(), err.error))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Mutex, OnceLock};

  static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

  fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .expect("env test lock poisoned")
  }

  struct EnvVarGuard {
    bookmarks_prev: Option<std::ffi::OsString>,
    history_prev: Option<std::ffi::OsString>,
  }

  impl EnvVarGuard {
    fn new() -> Self {
      Self {
        bookmarks_prev: std::env::var_os(BOOKMARKS_ENV_PATH),
        history_prev: std::env::var_os(HISTORY_ENV_PATH),
      }
    }
  }

  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      match &self.bookmarks_prev {
        Some(v) => std::env::set_var(BOOKMARKS_ENV_PATH, v),
        None => std::env::remove_var(BOOKMARKS_ENV_PATH),
      }
      match &self.history_prev {
        Some(v) => std::env::set_var(HISTORY_ENV_PATH, v),
        None => std::env::remove_var(HISTORY_ENV_PATH),
      }
    }
  }

  #[test]
  fn bookmarks_path_env_override_wins() {
    let _lock = lock_env();
    let _guard = EnvVarGuard::new();

    std::env::set_var(BOOKMARKS_ENV_PATH, "/tmp/fastr_bookmarks_override.json");
    assert_eq!(
      bookmarks_path(),
      PathBuf::from("/tmp/fastr_bookmarks_override.json")
    );
  }

  #[test]
  fn history_path_env_override_wins() {
    let _lock = lock_env();
    let _guard = EnvVarGuard::new();

    std::env::set_var(HISTORY_ENV_PATH, "/tmp/fastr_history_override.json");
    assert_eq!(
      history_path(),
      PathBuf::from("/tmp/fastr_history_override.json")
    );
  }

  #[test]
  fn load_missing_files_returns_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");

    let bookmarks = load_bookmarks(&bookmarks_path).unwrap();
    assert_eq!(bookmarks.source, LoadSource::Empty);
    assert_eq!(bookmarks.value, BookmarkStore::default());

    let history = load_history(&history_path).unwrap();
    assert_eq!(history.source, LoadSource::Empty);
    assert_eq!(history.value, GlobalHistoryStore::default());
  }

  #[test]
  fn save_atomic_writes_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");

    let mut bookmarks = BookmarkStore::default();
    assert!(bookmarks.toggle("https://a.example/", Some("a")));
    assert!(bookmarks.toggle("https://b.example/", Some("b")));
    save_bookmarks_atomic(&bookmarks_path, &bookmarks).unwrap();

    let mut history = GlobalHistoryStore::default();
    history.entries.push(GlobalHistoryEntry {
      url: "https://example.com/".to_string(),
      title: Some("Example".to_string()),
      visited_at_ms: Some(123),
      visit_count: 1,
    });
    save_history_atomic(&history_path, &history).unwrap();

    let loaded_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(loaded_bookmarks, bookmarks);

    let loaded_history: PersistedGlobalHistoryStore =
      serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    assert_eq!(loaded_history, PersistedGlobalHistoryStore::from_store(&history));
  }

  #[test]
  fn migrates_legacy_bookmarks_schemas() {
    use crate::ui::bookmarks::{BookmarkNode, BOOKMARK_STORE_VERSION};

    // Legacy windowed schema (pre-versioning): object without `version`.
    let legacy_typed = r#"{"urls":["https://example.com/"]}"#;
    let store = parse_bookmarks_json(legacy_typed).unwrap();
    assert_eq!(store.version, BOOKMARK_STORE_VERSION);
    assert_eq!(store.roots.len(), 1);
    let id = store.roots[0];
    match store.nodes.get(&id) {
      Some(BookmarkNode::Bookmark(entry)) => {
        assert_eq!(entry.url, "https://example.com/");
        // Legacy URLs stores use the URL as the title.
        assert_eq!(entry.title.as_deref(), Some("https://example.com/"));
        assert_eq!(entry.added_at_ms, 0);
      }
      other => panic!("expected a migrated root bookmark, got {other:?}"),
    }

    // Legacy headless schema: array of objects.
    let legacy_headless = r#"[{"title":"Example","url":"https://example.com"}]"#;
    let store = parse_bookmarks_json(legacy_headless).unwrap();
    assert_eq!(store.version, BOOKMARK_STORE_VERSION);
    assert_eq!(store.roots.len(), 1);
    let id = store.roots[0];
    match store.nodes.get(&id) {
      Some(BookmarkNode::Bookmark(entry)) => {
        assert_eq!(entry.url, "https://example.com");
        assert_eq!(entry.title.as_deref(), Some("Example"));
        assert_eq!(entry.added_at_ms, 0);
      }
      other => panic!("expected a migrated headless bookmark, got {other:?}"),
    }

    // Unsupported versions should error (do not silently ignore the version field).
    let unsupported = r#"{"version":999}"#;
    assert!(parse_bookmarks_json(unsupported).is_err());
  }

  #[test]
  fn migrates_legacy_history_schemas() {
    // Canonical v1 schema (versioned object).
    let v1 = r#"{"version":1,"entries":[{"url":"https://example.com/","visited_at_ms":5,"visit_count":2}]}"#;
    let store = parse_history_json(v1).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "https://example.com/");
    assert_eq!(store.entries[0].visited_at_ms, Some(5));
    assert_eq!(store.entries[0].visit_count, 2);

    // Legacy windowed schema (pre-versioning): object without `version`.
    let legacy_typed = r#"{"entries":[{"url":"https://example.com/","visited_at_ms":5}]}"#;
    let store = parse_history_json(legacy_typed).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "https://example.com/");
    assert_eq!(store.entries[0].visited_at_ms, Some(5));
    assert_eq!(store.entries[0].visit_count, 1);

    // Legacy headless schema: array of objects, `ts` instead of `visited_at_ms`.
    let legacy_headless = r#"[{"title":"Example","url":"https://example.com/","ts":123}]"#;
    let store = parse_history_json(legacy_headless).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "https://example.com/");
    assert_eq!(store.entries[0].title.as_deref(), Some("Example"));
    assert_eq!(store.entries[0].visited_at_ms, Some(123));
    assert_eq!(store.entries[0].visit_count, 1);

    // Unsupported versions should error (do not silently ignore the version field).
    let unsupported = r#"{"version":999,"entries":[]}"#;
    assert!(parse_history_json(unsupported).is_err());
  }
}
