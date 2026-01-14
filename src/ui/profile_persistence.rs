//! Browser UI profile persistence (bookmarks/history/downloads).
//!
//! This module is the single authoritative implementation for:
//! - Determining persistence paths (`*_path` helpers)
//! - Loading/saving bookmarks/history JSON
//! - On-disk schema versioning + migrations
//!
//! Both the windowed UI and `browser --headless-smoke` must use these helpers so the schema stays
//! in lockstep.

use crate::ui::bookmarks::BookmarkStore;
use crate::ui::browser_app::{DownloadEntry, DownloadStatus, DownloadsState};
use crate::ui::global_history::{GlobalHistoryEntry, GlobalHistoryStore};
use crate::ui::messages::{DownloadId, TabId};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const BOOKMARKS_ENV_PATH: &str = "FASTR_BROWSER_BOOKMARKS_PATH";
const HISTORY_ENV_PATH: &str = "FASTR_BROWSER_HISTORY_PATH";
const DOWNLOADS_ENV_PATH: &str = "FASTR_BROWSER_DOWNLOADS_PATH";

const BOOKMARKS_FILE_NAME: &str = "fastrender_bookmarks.json";
const HISTORY_FILE_NAME: &str = "fastrender_history.json";
const DOWNLOADS_FILE_NAME: &str = "fastrender_downloads.json";

const HISTORY_VERSION: u32 = 1;
const DOWNLOADS_VERSION: u32 = 1;

// Keep the downloads file bounded so a long-lived profile does not grow without limit.
const MAX_PERSISTED_DOWNLOADS: usize = 500;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PersistedDownloadStatus {
  Completed,
  Failed,
  Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedDownloadEntry {
  pub url: String,
  #[serde(default)]
  pub file_name: String,
  pub path: PathBuf,
  pub status: PersistedDownloadStatus,
  /// Unix epoch milliseconds when the download started, when known.
  #[serde(default)]
  pub started_at_ms: Option<u64>,
  /// Unix epoch milliseconds when the download finished/cancelled, when known.
  #[serde(default)]
  pub finished_at_ms: Option<u64>,
}

/// Versioned on-disk downloads schema.
///
/// The in-memory UI model is [`DownloadsState`] (which is intentionally not versioned); this
/// wrapper is the authoritative persisted schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedDownloadsStore {
  pub version: u32,
  #[serde(default)]
  pub entries: Vec<PersistedDownloadEntry>,
}

impl Default for PersistedDownloadsStore {
  fn default() -> Self {
    Self {
      version: DOWNLOADS_VERSION,
      entries: Vec::new(),
    }
  }
}

impl PersistedDownloadsStore {
  fn sanitized(mut self) -> Self {
    self.version = DOWNLOADS_VERSION;
    self.entries.retain(|e| !e.url.trim().is_empty());

    // Dedup by (path, url), keeping the newest occurrence (last-in-file wins).
    use std::collections::HashSet;
    let mut seen: HashSet<(PathBuf, String)> = HashSet::new();
    let mut deduped_rev: Vec<PersistedDownloadEntry> = Vec::with_capacity(self.entries.len());
    for entry in self.entries.into_iter().rev() {
      let key = (entry.path.clone(), entry.url.clone());
      if seen.insert(key) {
        deduped_rev.push(entry);
      }
    }
    deduped_rev.reverse();
    self.entries = deduped_rev;

    if self.entries.len() > MAX_PERSISTED_DOWNLOADS {
      let overflow = self.entries.len() - MAX_PERSISTED_DOWNLOADS;
      self.entries.drain(0..overflow);
    }
    self
  }

  pub fn from_state(state: &DownloadsState) -> Self {
    Self {
      version: DOWNLOADS_VERSION,
      entries: state
        .downloads
        .iter()
        .filter_map(|d| {
          let status = match d.status {
            DownloadStatus::Completed => PersistedDownloadStatus::Completed,
            DownloadStatus::Cancelled => PersistedDownloadStatus::Cancelled,
            DownloadStatus::Failed { .. } => PersistedDownloadStatus::Failed,
            // Do not persist in-progress downloads across restart.
            DownloadStatus::InProgress { .. } => return None,
          };

          Some(PersistedDownloadEntry {
            url: d.url.clone(),
            file_name: d.file_name.clone(),
            path: d.path.clone(),
            status,
            started_at_ms: d.started_at_ms,
            finished_at_ms: d.finished_at_ms,
          })
        })
        .collect(),
    }
    .sanitized()
  }

  pub fn into_state(self) -> DownloadsState {
    let mut out = DownloadsState::default();
    out.downloads = self
      .entries
      .into_iter()
      .map(|e| {
        let path = e.path;
        let path_display = path.display().to_string();
        DownloadEntry {
          download_id: DownloadId::new(),
          // Downloads restored from disk are not associated with any active tab; UIs should pick a
          // reasonable tab id (e.g. active tab) when retrying.
          tab_id: TabId(0),
          url: e.url,
          file_name: e.file_name,
          path,
          path_display,
          status: match e.status {
            PersistedDownloadStatus::Completed => DownloadStatus::Completed,
            PersistedDownloadStatus::Cancelled => DownloadStatus::Cancelled,
            PersistedDownloadStatus::Failed => DownloadStatus::Failed { error: String::new() },
          },
          started_at_ms: e.started_at_ms,
          finished_at_ms: e.finished_at_ms,
        }
      })
      .collect();
    out
  }
}

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
    let mut store = GlobalHistoryStore::default();
    store.entries = self.entries;
    store.normalize_in_place();
    store
  }
}

fn profile_path(env_key: &str, file_name: &str) -> PathBuf {
  profile_path_from_lookup(env_key, file_name, |k| std::env::var_os(k))
}

fn profile_path_from_lookup(
  env_key: &str,
  file_name: &str,
  mut get: impl FnMut(&str) -> Option<OsString>,
) -> PathBuf {
  if let Some(raw) = get(env_key) {
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

/// Determine the on-disk downloads file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_DOWNLOADS_PATH` env var (used by tests).
/// 2. A deterministic per-user config file (via `directories`).
/// 3. Fallback to `./fastrender_downloads.json` in the current working directory.
pub fn downloads_path() -> PathBuf {
  profile_path(DOWNLOADS_ENV_PATH, DOWNLOADS_FILE_NAME)
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

/// Attempt to read + parse a downloads file.
///
/// Missing files return [`LoadSource::Empty`] rather than an error.
pub fn load_downloads(path: &Path) -> Result<LoadOutcome<DownloadsState>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      return Ok(LoadOutcome {
        source: LoadSource::Empty,
        value: DownloadsState::default(),
      })
    }
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let store =
    parse_downloads_json(&data).map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
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
      let mut store = GlobalHistoryStore::default();
      store.entries = entries
        .into_iter()
        .map(|e| GlobalHistoryEntry {
          url: e.url,
          title: e.title,
          visited_at_ms: e.ts.unwrap_or(0),
          visit_count: e.visit_count.unwrap_or(1).max(1),
        })
        .collect();
      store.normalize_in_place();
      store
    }
  })
}

/// Parse a downloads JSON payload (v1 schema) into the in-memory [`DownloadsState`] model.
pub fn parse_downloads_json(raw: &str) -> Result<DownloadsState, String> {
  let parsed: PersistedDownloadsStore = serde_json::from_str(raw).map_err(|err| err.to_string())?;
  if parsed.version != DOWNLOADS_VERSION {
    return Err(format!(
      "unsupported downloads version {}; expected {}",
      parsed.version, DOWNLOADS_VERSION
    ));
  }
  Ok(parsed.sanitized().into_state())
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

/// Write the downloads file atomically (write temp file + rename).
pub fn save_downloads_atomic(path: &Path, downloads: &DownloadsState) -> Result<(), String> {
  let persisted = PersistedDownloadsStore::from_state(downloads);
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
  use std::collections::HashMap;
  use std::ffi::OsString;

  #[test]
  fn bookmarks_path_env_override_wins() {
    let mut env = HashMap::new();
    env.insert(
      BOOKMARKS_ENV_PATH,
      OsString::from("/tmp/fastr_bookmarks_override.json"),
    );
    assert_eq!(
      profile_path_from_lookup(BOOKMARKS_ENV_PATH, BOOKMARKS_FILE_NAME, |k| env.get(k).cloned()),
      PathBuf::from("/tmp/fastr_bookmarks_override.json")
    );
  }

  #[test]
  fn history_path_env_override_wins() {
    let mut env = HashMap::new();
    env.insert(
      HISTORY_ENV_PATH,
      OsString::from("/tmp/fastr_history_override.json"),
    );
    assert_eq!(
      profile_path_from_lookup(HISTORY_ENV_PATH, HISTORY_FILE_NAME, |k| env.get(k).cloned()),
      PathBuf::from("/tmp/fastr_history_override.json")
    );
  }

  #[test]
  fn downloads_path_env_override_wins() {
    let mut env = HashMap::new();
    env.insert(
      DOWNLOADS_ENV_PATH,
      OsString::from("/tmp/fastr_downloads_override.json"),
    );
    assert_eq!(
      profile_path_from_lookup(DOWNLOADS_ENV_PATH, DOWNLOADS_FILE_NAME, |k| env.get(k).cloned()),
      PathBuf::from("/tmp/fastr_downloads_override.json")
    );
  }

  #[test]
  fn load_missing_files_returns_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let bookmarks = load_bookmarks(&bookmarks_path).unwrap();
    assert_eq!(bookmarks.source, LoadSource::Empty);
    assert_eq!(bookmarks.value, BookmarkStore::default());

    let history = load_history(&history_path).unwrap();
    assert_eq!(history.source, LoadSource::Empty);
    assert_eq!(history.value, GlobalHistoryStore::default());

    let downloads = load_downloads(&downloads_path).unwrap();
    assert_eq!(downloads.source, LoadSource::Empty);
    assert_eq!(downloads.value, DownloadsState::default());
  }

  #[test]
  fn save_atomic_writes_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let mut bookmarks = BookmarkStore::default();
    assert!(bookmarks.toggle("https://a.example/", Some("a")));
    assert!(bookmarks.toggle("https://b.example/", Some("b")));
    save_bookmarks_atomic(&bookmarks_path, &bookmarks).unwrap();

    let mut history = GlobalHistoryStore::default();
    history.entries.push(GlobalHistoryEntry {
      url: "https://example.com/".to_string(),
      title: Some("Example".to_string()),
      visited_at_ms: 123,
      visit_count: 1,
    });
    save_history_atomic(&history_path, &history).unwrap();

    let mut downloads = DownloadsState::default();
    downloads.downloads.push(DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: "https://example.com/file.zip".to_string(),
      file_name: "file.zip".to_string(),
      path: PathBuf::from("/tmp/file.zip"),
      status: DownloadStatus::Completed,
      started_at_ms: Some(1),
      finished_at_ms: Some(2),
    });
    // In-progress downloads should not be persisted.
    downloads.downloads.push(DownloadEntry {
      download_id: DownloadId(2),
      tab_id: TabId(2),
      url: "https://example.com/inprogress".to_string(),
      file_name: "inprogress.bin".to_string(),
      path: PathBuf::from("/tmp/inprogress.bin"),
      status: DownloadStatus::InProgress {
        received_bytes: 5,
        total_bytes: Some(10),
      },
      started_at_ms: Some(3),
      finished_at_ms: None,
    });
    save_downloads_atomic(&downloads_path, &downloads).unwrap();

    let loaded_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(loaded_bookmarks, bookmarks);

    let loaded_history: PersistedGlobalHistoryStore =
      serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    assert_eq!(loaded_history, PersistedGlobalHistoryStore::from_store(&history));

    let loaded_downloads: PersistedDownloadsStore =
      serde_json::from_str(&std::fs::read_to_string(&downloads_path).unwrap()).unwrap();
    assert_eq!(
      loaded_downloads,
      PersistedDownloadsStore::from_state(&downloads)
    );
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
    assert_eq!(store.entries[0].visited_at_ms, 5);
    assert_eq!(store.entries[0].visit_count, 2);

    // Legacy windowed schema (pre-versioning): object without `version`.
    let legacy_typed = r#"{"entries":[{"url":"https://example.com/","visited_at_ms":5}]}"#;
    let store = parse_history_json(legacy_typed).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "https://example.com/");
    assert_eq!(store.entries[0].visited_at_ms, 5);
    assert_eq!(store.entries[0].visit_count, 1);

    // Legacy headless schema: array of objects, `ts` instead of `visited_at_ms`.
    let legacy_headless = r#"[{"title":"Example","url":"https://example.com/","ts":123}]"#;
    let store = parse_history_json(legacy_headless).unwrap();
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "https://example.com/");
    assert_eq!(store.entries[0].title.as_deref(), Some("Example"));
    assert_eq!(store.entries[0].visited_at_ms, 123);
    assert_eq!(store.entries[0].visit_count, 1);

    // Unsupported versions should error (do not silently ignore the version field).
    let unsupported = r#"{"version":999,"entries":[]}"#;
    assert!(parse_history_json(unsupported).is_err());
  }

  #[test]
  fn downloads_schema_versioning_and_roundtrip() {
    // Canonical v1 schema.
    let v1 = r#"{"version":1,"entries":[{"url":"https://example.com/","file_name":"a.bin","path":"/tmp/a.bin","status":"completed","started_at_ms":1,"finished_at_ms":2}]}"#;
    let store = parse_downloads_json(v1).unwrap();
    assert_eq!(store.downloads.len(), 1);
    let entry = &store.downloads[0];
    assert_eq!(entry.url, "https://example.com/");
    assert_eq!(entry.file_name, "a.bin");
    assert_eq!(entry.path, PathBuf::from("/tmp/a.bin"));
    assert!(matches!(entry.status, DownloadStatus::Completed));
    assert_eq!(entry.started_at_ms, Some(1));
    assert_eq!(entry.finished_at_ms, Some(2));

    // Unsupported versions should error.
    let unsupported = r#"{"version":999,"entries":[]}"#;
    assert!(parse_downloads_json(unsupported).is_err());
  }
}
