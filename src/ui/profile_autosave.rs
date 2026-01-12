#![cfg(feature = "browser_ui")]

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::ui::GlobalHistoryStore;
#[cfg(test)]
use crate::ui::GlobalHistoryEntry;

const BOOKMARKS_ENV_PATH: &str = "FASTR_BROWSER_BOOKMARKS_PATH";
const HISTORY_ENV_PATH: &str = "FASTR_BROWSER_HISTORY_PATH";
const BOOKMARKS_FILE_NAME: &str = "fastrender_bookmarks.json";
const HISTORY_FILE_NAME: &str = "fastrender_history.json";

const DEFAULT_BOOKMARKS_DEBOUNCE: Duration = Duration::from_secs(1);
const DEFAULT_HISTORY_DEBOUNCE: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BookmarkStore {
  #[serde(default)]
  pub urls: std::collections::BTreeSet<String>,
}

impl BookmarkStore {
  pub fn toggle_url(&mut self, url: &str) -> bool {
    if self.urls.remove(url) {
      false
    } else {
      self.urls.insert(url.to_string());
      true
    }
  }

  pub fn contains(&self, url: &str) -> bool {
    self.urls.contains(url)
  }
}

/// Determine the on-disk bookmarks file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_BOOKMARKS_PATH` env var (used by tests).
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

/// Determine the on-disk global history file location.
///
/// Order of precedence:
/// 1. `FASTR_BROWSER_HISTORY_PATH` env var (used by tests).
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

/// Attempt to read + parse a bookmarks file. Missing file is not an error.
pub fn load_bookmarks(path: &Path) -> Result<Option<BookmarkStore>, String> {
  load_json(path)
}

/// Attempt to read + parse a history file. Missing file is not an error.
pub fn load_history(path: &Path) -> Result<Option<GlobalHistoryStore>, String> {
  load_json(path)
}

/// Write the bookmarks file atomically (write temp file + rename).
pub fn save_bookmarks_atomic(path: &Path, bookmarks: &BookmarkStore) -> Result<(), String> {
  save_json_atomic(path, bookmarks)
}

/// Write the history file atomically (write temp file + rename).
pub fn save_history_atomic(path: &Path, history: &GlobalHistoryStore) -> Result<(), String> {
  save_json_atomic(path, history)
}

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
  let data = match std::fs::read_to_string(path) {
    Ok(data) => data,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
  };

  let parsed: T =
    serde_json::from_str(&data).map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
  Ok(Some(parsed))
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

#[derive(Debug)]
pub enum AutosaveMsg {
  UpdateBookmarks(BookmarkStore),
  UpdateHistory(GlobalHistoryStore),
  /// Test hook: force an immediate write of the latest pending snapshots.
  Flush(mpsc::Sender<()>),
  Shutdown,
}

#[derive(Debug)]
pub struct ProfileAutosaveHandle {
  tx: mpsc::Sender<AutosaveMsg>,
  join: Option<std::thread::JoinHandle<()>>,
}

impl ProfileAutosaveHandle {
  pub fn sender(&self) -> mpsc::Sender<AutosaveMsg> {
    self.tx.clone()
  }

  pub fn send(&self, msg: AutosaveMsg) -> Result<(), mpsc::SendError<AutosaveMsg>> {
    self.tx.send(msg)
  }

  pub fn spawn(bookmarks_path: PathBuf, history_path: PathBuf) -> Self {
    Self::spawn_with_debounce(
      bookmarks_path,
      history_path,
      DEFAULT_BOOKMARKS_DEBOUNCE,
      DEFAULT_HISTORY_DEBOUNCE,
    )
  }

  pub fn spawn_with_debounce(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    bookmarks_debounce: Duration,
    history_debounce: Duration,
  ) -> Self {
    let (tx, rx) = mpsc::channel::<AutosaveMsg>();
    let join = std::thread::Builder::new()
      .name("fastr_profile_autosave".to_string())
      .spawn(move || {
        autosave_worker_main(
          rx,
          bookmarks_path,
          history_path,
          bookmarks_debounce,
          history_debounce,
        );
      })
      .expect("failed to spawn profile autosave thread");

    Self {
      tx,
      join: Some(join),
    }
  }

  pub fn flush(&self, timeout: Duration) -> Result<(), String> {
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    self
      .tx
      .send(AutosaveMsg::Flush(ack_tx))
      .map_err(|_| "autosave thread disconnected".to_string())?;
    ack_rx
      .recv_timeout(timeout)
      .map_err(|err| format!("timed out waiting for autosave flush ack: {err}"))?;
    Ok(())
  }

  pub fn shutdown_with_timeout(mut self, timeout: Duration) {
    // Best-effort; ignore send errors during shutdown.
    let _ = self.tx.send(AutosaveMsg::Shutdown);

    let Some(join) = self.join.take() else {
      return;
    };

    let (done_tx, done_rx) = mpsc::channel::<std::thread::Result<()>>();
    let _ = std::thread::spawn(move || {
      let _ = done_tx.send(join.join());
    });

    match done_rx.recv_timeout(timeout) {
      Ok(Ok(())) => {}
      Ok(Err(_)) => {
        eprintln!("profile autosave thread panicked during shutdown");
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        eprintln!("timed out waiting for profile autosave thread to exit; shutting down anyway");
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        eprintln!("profile autosave join helper thread disconnected during shutdown");
      }
    }
  }
}

fn autosave_worker_main(
  rx: mpsc::Receiver<AutosaveMsg>,
  bookmarks_path: PathBuf,
  history_path: PathBuf,
  bookmarks_debounce: Duration,
  history_debounce: Duration,
) {
  let mut pending_bookmarks: Option<BookmarkStore> = None;
  let mut pending_history: Option<GlobalHistoryStore> = None;
  let mut next_bookmarks_write: Option<Instant> = None;
  let mut next_history_write: Option<Instant> = None;

  loop {
    let now = Instant::now();
    let next_deadline = match (next_bookmarks_write, next_history_write) {
      (Some(a), Some(b)) => Some(a.min(b)),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    };

    let msg = match next_deadline {
      Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(now)),
      None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };

    match msg {
      Ok(AutosaveMsg::UpdateBookmarks(bookmarks)) => {
        pending_bookmarks = Some(bookmarks);
        next_bookmarks_write = Some(Instant::now() + bookmarks_debounce);
      }
      Ok(AutosaveMsg::UpdateHistory(history)) => {
        pending_history = Some(history);
        next_history_write = Some(Instant::now() + history_debounce);
      }
      Ok(AutosaveMsg::Flush(done_tx)) => {
        flush_pending(
          &bookmarks_path,
          &history_path,
          &mut pending_bookmarks,
          &mut pending_history,
        );
        next_bookmarks_write = None;
        next_history_write = None;
        let _ = done_tx.send(());
      }
      Ok(AutosaveMsg::Shutdown) => {
        flush_pending(
          &bookmarks_path,
          &history_path,
          &mut pending_bookmarks,
          &mut pending_history,
        );
        break;
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        let now = Instant::now();
        if next_bookmarks_write.is_some_and(|t| now >= t) {
          if let Some(bookmarks) = pending_bookmarks.take() {
            if let Err(err) = save_bookmarks_atomic(&bookmarks_path, &bookmarks) {
              eprintln!(
                "failed to autosave bookmarks to {}: {err}",
                bookmarks_path.display()
              );
            }
          }
          next_bookmarks_write = None;
        }
        if next_history_write.is_some_and(|t| now >= t) {
          if let Some(history) = pending_history.take() {
            if let Err(err) = save_history_atomic(&history_path, &history) {
              eprintln!(
                "failed to autosave history to {}: {err}",
                history_path.display()
              );
            }
          }
          next_history_write = None;
        }
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        flush_pending(
          &bookmarks_path,
          &history_path,
          &mut pending_bookmarks,
          &mut pending_history,
        );
        break;
      }
    }
  }
}

fn flush_pending(
  bookmarks_path: &Path,
  history_path: &Path,
  pending_bookmarks: &mut Option<BookmarkStore>,
  pending_history: &mut Option<GlobalHistoryStore>,
) {
  if let Some(bookmarks) = pending_bookmarks.take() {
    if let Err(err) = save_bookmarks_atomic(bookmarks_path, &bookmarks) {
      eprintln!(
        "failed to autosave bookmarks to {}: {err}",
        bookmarks_path.display()
      );
    }
  }

  if let Some(history) = pending_history.take() {
    if let Err(err) = save_history_atomic(history_path, &history) {
      eprintln!(
        "failed to autosave history to {}: {err}",
        history_path.display()
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn flush_writes_last_update() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path.clone(),
      history_path.clone(),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    );

    autosave
      .send(AutosaveMsg::UpdateBookmarks(BookmarkStore {
        urls: ["https://a.example/".to_string()].into_iter().collect(),
      }))
      .unwrap();
    autosave
      .send(AutosaveMsg::UpdateBookmarks(BookmarkStore {
        urls: ["https://b.example/".to_string()].into_iter().collect(),
      }))
      .unwrap();

    autosave
      .send(AutosaveMsg::UpdateHistory(GlobalHistoryStore {
        entries: vec![GlobalHistoryEntry {
          url: "https://1.example/".to_string(),
          title: None,
          visited_at_ms: None,
        }],
      }))
      .unwrap();
    autosave
      .send(AutosaveMsg::UpdateHistory(GlobalHistoryStore {
        entries: vec![GlobalHistoryEntry {
          url: "https://2.example/".to_string(),
          title: Some("two".to_string()),
          visited_at_ms: Some(2),
        }],
      }))
      .unwrap();

    autosave.flush(Duration::from_millis(500)).unwrap();

    let saved_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(
      saved_bookmarks,
      BookmarkStore {
        urls: ["https://b.example/".to_string()].into_iter().collect(),
      }
    );

    let saved_history: GlobalHistoryStore =
      serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    assert_eq!(
      saved_history,
      GlobalHistoryStore {
        entries: vec![GlobalHistoryEntry {
          url: "https://2.example/".to_string(),
          title: Some("two".to_string()),
          visited_at_ms: Some(2),
        }],
      }
    );

    autosave.shutdown_with_timeout(Duration::from_millis(500));
  }

  #[test]
  fn shutdown_flushes_pending_updates() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path.clone(),
      history_path.clone(),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    );

    autosave
      .send(AutosaveMsg::UpdateBookmarks(BookmarkStore {
        urls: ["https://final.example/".to_string()]
          .into_iter()
          .collect(),
      }))
      .unwrap();

    autosave.shutdown_with_timeout(Duration::from_millis(500));

    let saved_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(
      saved_bookmarks,
      BookmarkStore {
        urls: ["https://final.example/".to_string()]
          .into_iter()
          .collect(),
      }
    );

    // History file should not exist (no history updates were sent).
    assert!(!history_path.exists());
  }
}
