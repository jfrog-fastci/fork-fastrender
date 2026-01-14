use crate::ui::profile_persistence::{
  save_bookmarks_atomic, save_downloads_atomic, save_history_atomic,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::bookmarks::{BookmarkDelta, BookmarkStore};
use super::browser_app::DownloadsState;
use super::global_history::{GlobalHistoryStore, HistoryVisitDelta};
#[cfg(test)]
use super::global_history::GlobalHistoryEntry;

const DEFAULT_BOOKMARKS_DEBOUNCE: Duration = Duration::from_secs(1);
const DEFAULT_HISTORY_DEBOUNCE: Duration = Duration::from_secs(8);
const DEFAULT_DOWNLOADS_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAutosaveError {
  Bookmarks { path: String, message: String },
  History { path: String, message: String },
}
#[derive(Debug)]
pub enum AutosaveMsg {
  UpdateBookmarks(BookmarkStore),
  ApplyBookmarkDeltas(Vec<BookmarkDelta>),
  UpdateHistory(GlobalHistoryStore),
  ApplyHistoryVisitDeltas(Vec<HistoryVisitDelta>),
  UpdateDownloads(DownloadsState),
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

  pub fn spawn(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    downloads_path: PathBuf,
  ) -> Result<Self, String> {
    Self::spawn_with_debounce(
      bookmarks_path,
      history_path,
      downloads_path,
      DEFAULT_BOOKMARKS_DEBOUNCE,
      DEFAULT_HISTORY_DEBOUNCE,
      DEFAULT_DOWNLOADS_DEBOUNCE,
    )
  }

  pub fn spawn_with_debounce(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    downloads_path: PathBuf,
    bookmarks_debounce: Duration,
    history_debounce: Duration,
    downloads_debounce: Duration,
  ) -> Result<Self, String> {
    Self::spawn_with_debounce_impl(
      bookmarks_path,
      history_path,
      downloads_path,
      bookmarks_debounce,
      history_debounce,
      downloads_debounce,
      None,
    )
  }

  pub fn spawn_with_error_channel(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    downloads_path: PathBuf,
  ) -> Result<(Self, mpsc::Receiver<ProfileAutosaveError>), String> {
    Self::spawn_with_debounce_with_error_channel(
      bookmarks_path,
      history_path,
      downloads_path,
      DEFAULT_BOOKMARKS_DEBOUNCE,
      DEFAULT_HISTORY_DEBOUNCE,
      DEFAULT_DOWNLOADS_DEBOUNCE,
    )
  }

  pub fn spawn_with_debounce_with_error_channel(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    downloads_path: PathBuf,
    bookmarks_debounce: Duration,
    history_debounce: Duration,
    downloads_debounce: Duration,
  ) -> Result<(Self, mpsc::Receiver<ProfileAutosaveError>), String> {
    let (error_tx, error_rx) = mpsc::channel::<ProfileAutosaveError>();
    let handle = Self::spawn_with_debounce_impl(
      bookmarks_path,
      history_path,
      downloads_path,
      bookmarks_debounce,
      history_debounce,
      downloads_debounce,
      Some(error_tx),
    )?;
    Ok((handle, error_rx))
  }

  fn spawn_with_debounce_impl(
    bookmarks_path: PathBuf,
    history_path: PathBuf,
    downloads_path: PathBuf,
    bookmarks_debounce: Duration,
    history_debounce: Duration,
    downloads_debounce: Duration,
    error_tx: Option<mpsc::Sender<ProfileAutosaveError>>,
  ) -> Result<Self, String> {
    #[cfg(any(test, debug_assertions))]
    {
      if should_force_spawn_error_for_test() {
        return Err("forced profile autosave spawn failure (test hook)".to_string());
      }
    }

    let (tx, rx) = mpsc::channel::<AutosaveMsg>();
    let join = std::thread::Builder::new()
      .name("fastr_profile_autosave".to_string())
      .spawn(move || {
        autosave_worker_main(
          rx,
          bookmarks_path,
          history_path,
          downloads_path,
          bookmarks_debounce,
          history_debounce,
          downloads_debounce,
          error_tx,
        );
      })
      .map_err(|err| format!("failed to spawn profile autosave thread: {err}"))?;

    Ok(Self {
      tx,
      join: Some(join),
    })
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
  downloads_path: PathBuf,
  bookmarks_debounce: Duration,
  history_debounce: Duration,
  downloads_debounce: Duration,
  error_tx: Option<mpsc::Sender<ProfileAutosaveError>>,
) {
  // Keep an in-memory snapshot of bookmarks so we can apply incremental deltas without cloning the
  // entire store for each update.
  let mut bookmarks_state: BookmarkStore =
    match crate::ui::profile_persistence::load_bookmarks(&bookmarks_path) {
      Ok(outcome) => outcome.value,
      Err(err) => {
        eprintln!(
          "failed to load bookmarks from {} for autosave baseline: {err}",
          bookmarks_path.display()
        );
        BookmarkStore::default()
      }
    };
  let mut bookmarks_dirty = false;
  // Keep an in-memory snapshot of history so we can apply incremental visit deltas without
  // repeatedly cloning the full store on the UI thread.
  let mut history_state: GlobalHistoryStore =
    match crate::ui::profile_persistence::load_history(&history_path) {
      Ok(outcome) => outcome.value,
      Err(err) => {
        eprintln!(
          "failed to load history from {} for autosave baseline: {err}",
          history_path.display()
        );
        GlobalHistoryStore::default()
      }
    };
  let mut history_dirty = false;
  let mut pending_downloads: Option<DownloadsState> = None;
  let mut next_bookmarks_write: Option<Instant> = None;
  let mut next_history_write: Option<Instant> = None;
  let mut next_downloads_write: Option<Instant> = None;

  loop {
    let now = Instant::now();
    let next_deadline = match (next_bookmarks_write, next_history_write, next_downloads_write) {
      (Some(a), Some(b), Some(c)) => Some(a.min(b).min(c)),
      (Some(a), Some(b), None) => Some(a.min(b)),
      (Some(a), None, Some(c)) => Some(a.min(c)),
      (None, Some(b), Some(c)) => Some(b.min(c)),
      (Some(a), None, None) => Some(a),
      (None, Some(b), None) => Some(b),
      (None, None, Some(c)) => Some(c),
      (None, None, None) => None,
    };

    let msg = match next_deadline {
      Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(now)),
      None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };

    match msg {
      Ok(AutosaveMsg::UpdateBookmarks(bookmarks)) => {
        bookmarks_state = bookmarks;
        bookmarks_dirty = true;
        next_bookmarks_write = Some(Instant::now() + bookmarks_debounce);
      }
      Ok(AutosaveMsg::ApplyBookmarkDeltas(deltas)) => {
        if !deltas.is_empty() {
          if let Err(err) = bookmarks_state.apply_deltas(&deltas) {
            eprintln!("failed to apply bookmark deltas in autosave worker: {err:?}");
          } else {
            bookmarks_dirty = true;
            next_bookmarks_write = Some(Instant::now() + bookmarks_debounce);
          }
        }
      }
      Ok(AutosaveMsg::UpdateHistory(history)) => {
        history_state = history;
        history_dirty = true;
        next_history_write = Some(Instant::now() + history_debounce);
      }
      Ok(AutosaveMsg::ApplyHistoryVisitDeltas(deltas)) => {
        if !deltas.is_empty() && history_state.apply_visit_deltas(&deltas) {
          history_dirty = true;
          next_history_write = Some(Instant::now() + history_debounce);
        }
      }
      Ok(AutosaveMsg::UpdateDownloads(downloads)) => {
        pending_downloads = Some(downloads);
        next_downloads_write = Some(Instant::now() + downloads_debounce);
      }
      Ok(AutosaveMsg::Flush(done_tx)) => {
        flush_pending(
          &bookmarks_path,
          &history_path,
          &downloads_path,
          &bookmarks_state,
          &mut bookmarks_dirty,
          &history_state,
          &mut history_dirty,
          &mut pending_downloads,
          error_tx.as_ref(),
        );
        next_bookmarks_write = None;
        next_history_write = None;
        next_downloads_write = None;
        let _ = done_tx.send(());
      }
      Ok(AutosaveMsg::Shutdown) => {
        flush_pending(
          &bookmarks_path,
          &history_path,
          &downloads_path,
          &bookmarks_state,
          &mut bookmarks_dirty,
          &history_state,
          &mut history_dirty,
          &mut pending_downloads,
          error_tx.as_ref(),
        );
        break;
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        let now = Instant::now();
        if next_bookmarks_write.is_some_and(|t| now >= t) && bookmarks_dirty {
          if let Err(err) = save_bookmarks_atomic(&bookmarks_path, &bookmarks_state) {
            eprintln!(
              "failed to autosave bookmarks to {}: {err}",
              bookmarks_path.display()
            );
            if let Some(tx) = error_tx.as_ref() {
              let _ = tx.send(ProfileAutosaveError::Bookmarks {
                path: bookmarks_path.to_string_lossy().to_string(),
                message: err,
              });
            }
          }
          bookmarks_dirty = false;
          next_bookmarks_write = None;
        }

        if next_history_write.is_some_and(|t| now >= t) && history_dirty {
          if let Err(err) = save_history_atomic(&history_path, &history_state) {
            eprintln!("failed to autosave history to {}: {err}", history_path.display());
            if let Some(tx) = error_tx.as_ref() {
              let _ = tx.send(ProfileAutosaveError::History {
                path: history_path.to_string_lossy().to_string(),
                message: err,
              });
            }
          }
          history_dirty = false;
          next_history_write = None;
        }

        if next_downloads_write.is_some_and(|t| now >= t) {
          if let Some(downloads) = pending_downloads.take() {
            if let Err(err) = save_downloads_atomic(&downloads_path, &downloads) {
              eprintln!(
                "failed to autosave downloads to {}: {err}",
                downloads_path.display()
              );
            }
          }
          next_downloads_write = None;
        }
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
}

fn flush_pending(
  bookmarks_path: &Path,
  history_path: &Path,
  downloads_path: &Path,
  bookmarks_state: &BookmarkStore,
  bookmarks_dirty: &mut bool,
  history_state: &GlobalHistoryStore,
  history_dirty: &mut bool,
  pending_downloads: &mut Option<DownloadsState>,
  error_tx: Option<&mpsc::Sender<ProfileAutosaveError>>,
) {
  if *bookmarks_dirty {
    if let Err(err) = save_bookmarks_atomic(bookmarks_path, bookmarks_state) {
      eprintln!(
        "failed to autosave bookmarks to {}: {err}",
        bookmarks_path.display()
      );
      if let Some(tx) = error_tx {
        let _ = tx.send(ProfileAutosaveError::Bookmarks {
          path: bookmarks_path.to_string_lossy().to_string(),
          message: err,
        });
      }
    }
    *bookmarks_dirty = false;
  }

  if *history_dirty {
    if let Err(err) = save_history_atomic(history_path, history_state) {
      eprintln!("failed to autosave history to {}: {err}", history_path.display());
      if let Some(tx) = error_tx {
        let _ = tx.send(ProfileAutosaveError::History {
          path: history_path.to_string_lossy().to_string(),
          message: err,
        });
      }
    }
    *history_dirty = false;
  }

  if let Some(downloads) = pending_downloads.take() {
    if let Err(err) = save_downloads_atomic(downloads_path, &downloads) {
      eprintln!(
        "failed to autosave downloads to {}: {err}",
        downloads_path.display()
      );
    }
  }
}

#[cfg(any(test, debug_assertions))]
fn should_force_spawn_error_for_test() -> bool {
  // Deterministic test hook for forcing `ProfileAutosaveHandle::spawn*` to fail.
  //
  // This is used by unit/integration tests to exercise code paths that disable profile autosave
  // (e.g., browser chrome startup toasts). It intentionally does *not* rely on a global env var in
  // tests, since the test runner executes tests in parallel and env vars are process-global.
  #[cfg(test)]
  {
    if FORCE_SPAWN_ERROR_FOR_TEST.with(|cell| cell.get()) {
      return true;
    }
  }

  // Optional process-wide override for debug builds (useful for manual testing).
  std::env::var("FASTR_FORCE_PROFILE_AUTOSAVE_SPAWN_ERROR")
    .ok()
    .is_some_and(|v| {
      let v = v.trim();
      !v.is_empty() && v != "0"
    })
}

#[cfg(test)]
thread_local! {
  static FORCE_SPAWN_ERROR_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct ForceSpawnErrorForTestGuard {
  prev: bool,
}

#[cfg(test)]
impl ForceSpawnErrorForTestGuard {
  fn new() -> Self {
    let prev = FORCE_SPAWN_ERROR_FOR_TEST.with(|cell| {
      let prev = cell.get();
      cell.set(true);
      prev
    });
    Self { prev }
  }
}

#[cfg(test)]
impl Drop for ForceSpawnErrorForTestGuard {
  fn drop(&mut self) {
    FORCE_SPAWN_ERROR_FOR_TEST.with(|cell| cell.set(self.prev));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::messages::{DownloadId, TabId};
  use crate::ui::profile_persistence::{PersistedDownloadsStore, PersistedGlobalHistoryStore};
  use std::path::PathBuf;

  #[test]
  fn flush_writes_last_update() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path.clone(),
      history_path.clone(),
      downloads_path.clone(),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    )
    .unwrap();

    let mut bookmarks_a = BookmarkStore::default();
    bookmarks_a.toggle("https://a.example/", Some("a"));
    let mut bookmarks_b = BookmarkStore::default();
    bookmarks_b.toggle("https://b.example/", Some("b"));

    autosave.send(AutosaveMsg::UpdateBookmarks(bookmarks_a)).unwrap();
    autosave
      .send(AutosaveMsg::UpdateBookmarks(bookmarks_b.clone()))
      .unwrap();

    autosave
      .send(AutosaveMsg::UpdateHistory({
        let mut history = GlobalHistoryStore::default();
        history.entries = vec![GlobalHistoryEntry {
          url: "https://1.example/".to_string(),
          title: None,
          visited_at_ms: 1,
          visit_count: 1,
        }];
        history
      }))
      .unwrap();
    autosave
      .send(AutosaveMsg::UpdateHistory({
        let mut history = GlobalHistoryStore::default();
        history.entries = vec![GlobalHistoryEntry {
          url: "https://2.example/".to_string(),
          title: Some("two".to_string()),
          visited_at_ms: 2,
          visit_count: 1,
        }];
        history
      }))
      .unwrap();

    let mut downloads_a = DownloadsState::default();
    downloads_a.downloads.push(crate::ui::browser_app::DownloadEntry {
      download_id: DownloadId(1),
      tab_id: TabId(1),
      url: "https://a.example/file".to_string(),
      file_name: "file".to_string(),
      path: PathBuf::from("/tmp/a"),
      path_display: "/tmp/a".to_string(),
      status: crate::ui::browser_app::DownloadStatus::Completed,
      started_at_ms: Some(1),
      finished_at_ms: Some(2),
    });
    let mut downloads_b = DownloadsState::default();
    downloads_b.downloads.push(crate::ui::browser_app::DownloadEntry {
      download_id: DownloadId(2),
      tab_id: TabId(2),
      url: "https://b.example/file".to_string(),
      file_name: "file".to_string(),
      path: PathBuf::from("/tmp/b"),
      path_display: "/tmp/b".to_string(),
      status: crate::ui::browser_app::DownloadStatus::Cancelled,
      started_at_ms: Some(3),
      finished_at_ms: Some(4),
    });
    autosave
      .send(AutosaveMsg::UpdateDownloads(downloads_a))
      .unwrap();
    autosave
      .send(AutosaveMsg::UpdateDownloads(downloads_b.clone()))
      .unwrap();

    autosave.flush(Duration::from_millis(500)).unwrap();

    let saved_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(saved_bookmarks, bookmarks_b);

    let saved_history: PersistedGlobalHistoryStore =
      serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    let mut expected = GlobalHistoryStore::default();
    expected.entries = vec![GlobalHistoryEntry {
      url: "https://2.example/".to_string(),
      title: Some("two".to_string()),
      visited_at_ms: 2,
      visit_count: 1,
    }];
    assert_eq!(saved_history, PersistedGlobalHistoryStore::from_store(&expected));

    let saved_downloads: PersistedDownloadsStore =
      serde_json::from_str(&std::fs::read_to_string(&downloads_path).unwrap()).unwrap();
    assert_eq!(saved_downloads, PersistedDownloadsStore::from_state(&downloads_b));

    autosave.shutdown_with_timeout(Duration::from_millis(500));
  }

  #[test]
  fn shutdown_flushes_pending_updates() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path.clone(),
      history_path.clone(),
      downloads_path.clone(),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    )
    .unwrap();

    let mut bookmarks = BookmarkStore::default();
    bookmarks.toggle("https://final.example/", Some("final"));
    autosave
      .send(AutosaveMsg::UpdateBookmarks(bookmarks.clone()))
      .unwrap();

    autosave.shutdown_with_timeout(Duration::from_millis(500));

    let saved_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(saved_bookmarks, bookmarks);

    // History file should not exist (no history updates were sent).
    assert!(!history_path.exists());
    // Downloads file should not exist (no downloads updates were sent).
    assert!(!downloads_path.exists());
  }

  #[test]
  fn apply_bookmark_deltas_writes_updated_store() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path.clone(),
      history_path,
      downloads_path,
      Duration::from_secs(3600),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    )
    .unwrap();

    let mut expected = BookmarkStore::default();
    let mut deltas = Vec::new();
    expected
      .add_with_deltas(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        None,
        &mut deltas,
      )
      .unwrap();

    autosave
      .send(AutosaveMsg::ApplyBookmarkDeltas(deltas))
      .unwrap();

    autosave.flush(Duration::from_millis(500)).unwrap();

    let saved_bookmarks: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).unwrap()).unwrap();
    assert_eq!(saved_bookmarks, expected);

    autosave.shutdown_with_timeout(Duration::from_millis(500));
  }

  #[test]
  fn apply_history_visit_deltas_writes_updated_store() {
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let autosave = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path,
      history_path.clone(),
      downloads_path,
      Duration::from_secs(3600),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    )
    .unwrap();

    let delta_a = HistoryVisitDelta {
      url: "https://a.example/".to_string(),
      title: Some("A1".to_string()),
      visited_at_ms: 1,
    };
    let delta_b = HistoryVisitDelta {
      url: "https://b.example/".to_string(),
      title: None,
      visited_at_ms: 2,
    };
    let delta_a2 = HistoryVisitDelta {
      url: "https://a.example/".to_string(),
      title: Some("A2".to_string()),
      visited_at_ms: 3,
    };
    autosave
      .send(AutosaveMsg::ApplyHistoryVisitDeltas(vec![
        delta_a.clone(),
        delta_b.clone(),
        delta_a2.clone(),
      ]))
      .unwrap();

    autosave.flush(Duration::from_millis(500)).unwrap();

    let saved_history: PersistedGlobalHistoryStore =
      serde_json::from_str(&std::fs::read_to_string(&history_path).unwrap()).unwrap();
    let mut expected = GlobalHistoryStore::default();
    expected.apply_visit_delta(&delta_a);
    expected.apply_visit_delta(&delta_b);
    expected.apply_visit_delta(&delta_a2);
    assert_eq!(saved_history, PersistedGlobalHistoryStore::from_store(&expected));

    autosave.shutdown_with_timeout(Duration::from_millis(500));
  }

  #[test]
  fn disabled_handle_does_not_panic() {
    let (tx, rx) = mpsc::channel::<AutosaveMsg>();
    drop(rx);
    let autosave = ProfileAutosaveHandle { tx, join: None };

    let err = autosave.flush(Duration::from_millis(10)).unwrap_err();
    assert!(err.contains("autosave thread disconnected"));

    autosave.shutdown_with_timeout(Duration::from_millis(10));
  }

  #[test]
  fn reports_autosave_errors_via_channel() {
    let dir = tempfile::tempdir().unwrap();

    // Create a file (not a directory) so `create_dir_all(parent)` fails deterministically.
    let not_a_dir = dir.path().join("not_a_dir");
    std::fs::write(&not_a_dir, b"not a directory").unwrap();

    let bookmarks_path = not_a_dir.join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");

    let (autosave, error_rx) = ProfileAutosaveHandle::spawn_with_debounce_with_error_channel(
      bookmarks_path.clone(),
      history_path,
      downloads_path,
      Duration::from_secs(3600),
      Duration::from_secs(3600),
      Duration::from_secs(3600),
    )
    .unwrap();

    let mut bookmarks = BookmarkStore::default();
    bookmarks.toggle("https://example.com/", Some("Example"));
    autosave.send(AutosaveMsg::UpdateBookmarks(bookmarks)).unwrap();

    // Force an immediate write attempt (which should fail).
    autosave.flush(Duration::from_millis(500)).unwrap();

    let err = error_rx
      .recv_timeout(Duration::from_millis(500))
      .expect("expected autosave error");
    match err {
      ProfileAutosaveError::Bookmarks { path, message } => {
        assert_eq!(path, bookmarks_path.to_string_lossy().to_string());
        assert!(
          message.contains("failed to create"),
          "unexpected error message: {message:?}"
        );
      }
      other => panic!("expected bookmarks error, got: {other:?}"),
    }

    autosave.shutdown_with_timeout(Duration::from_millis(500));
  }

  #[test]
  fn spawn_can_be_forced_to_fail_in_tests() {
    let _guard = ForceSpawnErrorForTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let bookmarks_path = dir.path().join("bookmarks.json");
    let history_path = dir.path().join("history.json");
    let downloads_path = dir.path().join("downloads.json");
    let err = ProfileAutosaveHandle::spawn_with_debounce(
      bookmarks_path,
      history_path,
      downloads_path,
      Duration::ZERO,
      Duration::ZERO,
      Duration::ZERO,
    )
    .unwrap_err();
    assert!(err.contains("forced profile autosave spawn failure"));

    // Ensure callers can turn the failure into a user-facing startup toast.
    let toast = crate::ui::startup_toasts::profile_autosave_start_failed_toast(&err);
    assert_eq!(toast.kind, crate::ui::ToastKind::Warning);
    assert!(toast.text.contains("Profile autosave disabled"));
  }
}
