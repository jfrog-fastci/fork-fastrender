//! Background-thread bookmarks import/export jobs.
//!
//! The egui UI thread should never block on filesystem I/O or large JSON
//! (de)serialization. This module provides a tiny state machine that spawns a
//! worker thread for bookmarks import/export and reports completion back to the
//! caller via an `mpsc` channel.

use std::path::Path;
use std::sync::mpsc;

use crate::ui::bookmarks::{BookmarkStore, BookmarkStoreMigration};
use crate::ui::profile_persistence::save_bookmarks_atomic;

#[derive(Debug)]
pub enum BookmarksIoJob {
  Idle,
  Exporting {
    path: String,
    rx: mpsc::Receiver<BookmarksIoJobUpdate>,
  },
  Importing {
    path: String,
    rx: mpsc::Receiver<BookmarksIoJobUpdate>,
  },
  ExportingJson {
    rx: mpsc::Receiver<BookmarksIoJobUpdate>,
  },
  ImportingJson {
    rx: mpsc::Receiver<BookmarksIoJobUpdate>,
  },
  Done,
  Error {
    error: String,
  },
}

impl Default for BookmarksIoJob {
  fn default() -> Self {
    Self::Idle
  }
}

#[derive(Debug)]
pub enum BookmarksIoJobUpdate {
  ExportFinished {
    path: String,
    /// `Err` is the full UI-facing error string (`state.error`).
    result: Result<(), String>,
  },
  ExportJsonFinished {
    /// `Err` is the full UI-facing error string (`state.error`).
    result: Result<String, String>,
  },
  ImportFinished {
    path: String,
    /// `Err` is the full UI-facing error string (`state.error`).
    result: Result<(BookmarkStore, BookmarkStoreMigration), String>,
  },
  ImportJsonFinished {
    /// `Err` is the full UI-facing error string (`state.error`).
    result: Result<(BookmarkStore, BookmarkStoreMigration), String>,
  },
}

impl BookmarksIoJob {
  pub fn is_busy(&self) -> bool {
    matches!(
      self,
      Self::Exporting { .. }
        | Self::Importing { .. }
        | Self::ExportingJson { .. }
        | Self::ImportingJson { .. }
    )
  }

  pub fn is_exporting(&self) -> bool {
    matches!(self, Self::Exporting { .. })
  }

  pub fn is_importing(&self) -> bool {
    matches!(self, Self::Importing { .. })
  }

  pub fn is_exporting_json(&self) -> bool {
    matches!(self, Self::ExportingJson { .. })
  }

  pub fn is_importing_json(&self) -> bool {
    matches!(self, Self::ImportingJson { .. })
  }

  pub fn start_export(&mut self, path: String, store: BookmarkStore) -> Result<(), String> {
    if self.is_busy() {
      return Err("bookmarks IO job already running".to_string());
    }

    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();
    let path_for_thread = path.clone();

    std::thread::Builder::new()
      .name("fastr_bookmarks_export".to_string())
      .spawn(move || {
        let result = save_bookmarks_atomic(Path::new(&path_for_thread), &store)
          .map_err(|err| format!("Failed to export bookmarks: {err}"));
        let _ = tx.send(BookmarksIoJobUpdate::ExportFinished {
          path: path_for_thread,
          result,
        });
      })
      .map_err(|err| format!("Failed to export bookmarks: failed to spawn worker thread: {err}"))?;

    *self = Self::Exporting { path, rx };
    Ok(())
  }

  pub fn start_export_json(&mut self, store: BookmarkStore) -> Result<(), String> {
    if self.is_busy() {
      return Err("bookmarks IO job already running".to_string());
    }

    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();

    std::thread::Builder::new()
      .name("fastr_bookmarks_export_json".to_string())
      .spawn(move || {
        let result = serde_json::to_string_pretty(&store)
          .map_err(|err| format!("Failed to export bookmarks: {err}"));
        let _ = tx.send(BookmarksIoJobUpdate::ExportJsonFinished { result });
      })
      .map_err(|err| {
        format!(
          "Failed to export bookmarks: failed to spawn worker thread: {err}"
        )
      })?;

    *self = Self::ExportingJson { rx };
    Ok(())
  }

  pub fn start_import(&mut self, path: String) -> Result<(), String> {
    if self.is_busy() {
      return Err("bookmarks IO job already running".to_string());
    }

    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();
    let path_for_thread = path.clone();

    std::thread::Builder::new()
      .name("fastr_bookmarks_import".to_string())
      .spawn(move || {
        let result = match std::fs::read_to_string(&path_for_thread) {
          Ok(json) => match BookmarkStore::from_json_str_migrating(&json) {
            Ok((imported, migration)) => Ok((imported, migration)),
            Err(err) => Err(format!("Failed to import bookmarks: {err:?}")),
          },
          Err(err) => Err(format!("Failed to read {path_for_thread:?}: {err}")),
        };
        let _ = tx.send(BookmarksIoJobUpdate::ImportFinished {
          path: path_for_thread,
          result,
        });
      })
      .map_err(|err| format!("Failed to import bookmarks: failed to spawn worker thread: {err}"))?;

    *self = Self::Importing { path, rx };
    Ok(())
  }

  pub fn start_import_json(&mut self, json: String) -> Result<(), String> {
    if self.is_busy() {
      return Err("bookmarks IO job already running".to_string());
    }

    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();

    std::thread::Builder::new()
      .name("fastr_bookmarks_import_json".to_string())
      .spawn(move || {
        let result = match BookmarkStore::from_json_str_migrating(&json) {
          Ok((imported, migration)) => Ok((imported, migration)),
          Err(err) => Err(format!("Failed to import bookmarks: {err:?}")),
        };
        let _ = tx.send(BookmarksIoJobUpdate::ImportJsonFinished { result });
      })
      .map_err(|err| {
        format!(
          "Failed to import bookmarks: failed to spawn worker thread: {err}"
        )
      })?;

    *self = Self::ImportingJson { rx };
    Ok(())
  }

  /// Non-blocking poll for a completed job result.
  ///
  /// Returns `Some` at most once per job.
  pub fn poll(&mut self) -> Option<BookmarksIoJobUpdate> {
    let state = std::mem::replace(self, Self::Idle);
    match state {
      Self::Exporting { path, rx } => match rx.try_recv() {
        Ok(update) => {
          if let BookmarksIoJobUpdate::ExportFinished {
            result: Err(err), ..
          } = &update
          {
            *self = Self::Error { error: err.clone() };
          } else {
            *self = Self::Done;
          }
          Some(update)
        }
        Err(mpsc::TryRecvError::Empty) => {
          *self = Self::Exporting { path, rx };
          None
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          let path = path.clone();
          let err = "Bookmarks export job disconnected.".to_string();
          *self = Self::Error { error: err.clone() };
          Some(BookmarksIoJobUpdate::ExportFinished {
            path,
            result: Err(err),
          })
        }
      },
      Self::Importing { path, rx } => match rx.try_recv() {
        Ok(update) => {
          if let BookmarksIoJobUpdate::ImportFinished {
            result: Err(err), ..
          } = &update
          {
            *self = Self::Error { error: err.clone() };
          } else {
            *self = Self::Done;
          }
          Some(update)
        }
        Err(mpsc::TryRecvError::Empty) => {
          *self = Self::Importing { path, rx };
          None
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          let path = path.clone();
          let err = "Bookmarks import job disconnected.".to_string();
          *self = Self::Error { error: err.clone() };
          Some(BookmarksIoJobUpdate::ImportFinished {
            path,
            result: Err(err),
          })
        }
      },
      Self::ExportingJson { rx } => match rx.try_recv() {
        Ok(update) => {
          if let BookmarksIoJobUpdate::ExportJsonFinished { result: Err(err) } = &update {
            *self = Self::Error { error: err.clone() };
          } else {
            *self = Self::Done;
          }
          Some(update)
        }
        Err(mpsc::TryRecvError::Empty) => {
          *self = Self::ExportingJson { rx };
          None
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          let err = "Bookmarks JSON export job disconnected.".to_string();
          *self = Self::Error { error: err.clone() };
          Some(BookmarksIoJobUpdate::ExportJsonFinished {
            result: Err(err),
          })
        }
      },
      Self::ImportingJson { rx } => match rx.try_recv() {
        Ok(update) => {
          if let BookmarksIoJobUpdate::ImportJsonFinished { result: Err(err) } = &update {
            *self = Self::Error { error: err.clone() };
          } else {
            *self = Self::Done;
          }
          Some(update)
        }
        Err(mpsc::TryRecvError::Empty) => {
          *self = Self::ImportingJson { rx };
          None
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          let err = "Bookmarks JSON import job disconnected.".to_string();
          *self = Self::Error { error: err.clone() };
          Some(BookmarksIoJobUpdate::ImportJsonFinished {
            result: Err(err),
          })
        }
      },
      other => {
        *self = other;
        None
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{Duration, Instant};

  fn wait_for_update(job: &mut BookmarksIoJob) -> BookmarksIoJobUpdate {
    let start = Instant::now();
    loop {
      if let Some(update) = job.poll() {
        return update;
      }
      assert!(
        start.elapsed() < Duration::from_secs(2),
        "timed out waiting for job completion"
      );
      std::thread::sleep(Duration::from_millis(5));
    }
  }

  #[test]
  fn export_job_writes_file_and_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bookmarks.json");

    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://example.com/", Some("Example")));

    let mut job = BookmarksIoJob::default();
    job
      .start_export(path.to_string_lossy().to_string(), store.clone())
      .unwrap();
    assert!(job.is_busy());

    let update = wait_for_update(&mut job);
    match update {
      BookmarksIoJobUpdate::ExportFinished { result, .. } => assert!(result.is_ok()),
      other => panic!("expected export completion, got {other:?}"),
    }
    assert!(matches!(job, BookmarksIoJob::Done));

    let loaded: BookmarkStore =
      serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(loaded, store);
  }

  #[test]
  fn export_job_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let parent_file = dir.path().join("not_a_dir");
    std::fs::write(&parent_file, b"").unwrap();
    let path = parent_file.join("bookmarks.json");

    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://example.com/", Some("Example")));

    let mut job = BookmarksIoJob::default();
    job
      .start_export(path.to_string_lossy().to_string(), store)
      .unwrap();

    let update = wait_for_update(&mut job);
    match update {
      BookmarksIoJobUpdate::ExportFinished {
        result: Err(err), ..
      } => {
        assert!(err.contains("Failed to export bookmarks"));
      }
      other => panic!("expected export error, got {other:?}"),
    }
    assert!(matches!(job, BookmarksIoJob::Error { .. }));
  }

  #[test]
  fn import_job_reads_file_and_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bookmarks.json");

    let mut expected = BookmarkStore::default();
    assert!(expected.toggle("https://a.example/", Some("A")));
    assert!(expected.toggle("https://b.example/", Some("B")));
    std::fs::write(&path, serde_json::to_string_pretty(&expected).unwrap()).unwrap();

    let mut job = BookmarksIoJob::default();
    job
      .start_import(path.to_string_lossy().to_string())
      .unwrap();

    let update = wait_for_update(&mut job);
    let (imported, migration) = match update {
      BookmarksIoJobUpdate::ImportFinished {
        result: Ok((store, migration)),
        ..
      } => (store, migration),
      other => panic!("expected import completion, got {other:?}"),
    };
    assert_eq!(migration, BookmarkStoreMigration::None);

    // Apply the job output to a store (this is what the UI does).
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://different.example/", Some("Different")));
    store = imported;

    assert_eq!(store, expected);
    assert!(matches!(job, BookmarksIoJob::Done));
  }

  #[test]
  fn import_job_reports_failure_and_does_not_update_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bookmarks.json");
    std::fs::write(&path, "not valid json").unwrap();

    let mut job = BookmarksIoJob::default();
    job
      .start_import(path.to_string_lossy().to_string())
      .unwrap();

    let update = wait_for_update(&mut job);
    match update {
      BookmarksIoJobUpdate::ImportFinished {
        result: Err(err), ..
      } => {
        assert!(err.contains("Failed to import bookmarks"));
      }
      other => panic!("expected import error, got {other:?}"),
    }
    assert!(matches!(job, BookmarksIoJob::Error { .. }));
  }

  #[test]
  fn export_json_job_serializes_store_and_reports_success() {
    let mut store = BookmarkStore::default();
    assert!(store.toggle("https://example.com/", Some("Example")));

    let mut job = BookmarksIoJob::default();
    job.start_export_json(store.clone()).unwrap();

    let update = wait_for_update(&mut job);
    let json = match update {
      BookmarksIoJobUpdate::ExportJsonFinished { result: Ok(json) } => json,
      other => panic!("expected export json completion, got {other:?}"),
    };
    assert!(matches!(job, BookmarksIoJob::Done));

    let (decoded, migration) = BookmarkStore::from_json_str_migrating(&json).unwrap();
    assert_eq!(migration, BookmarkStoreMigration::None);
    assert_eq!(decoded, store);
  }

  #[test]
  fn import_json_job_parses_json_and_reports_success() {
    let mut expected = BookmarkStore::default();
    assert!(expected.toggle("https://a.example/", Some("A")));
    assert!(expected.toggle("https://b.example/", Some("B")));
    let json = serde_json::to_string_pretty(&expected).unwrap();

    let mut job = BookmarksIoJob::default();
    job.start_import_json(json).unwrap();

    let update = wait_for_update(&mut job);
    let (imported, migration) = match update {
      BookmarksIoJobUpdate::ImportJsonFinished {
        result: Ok((store, migration)),
      } => (store, migration),
      other => panic!("expected import json completion, got {other:?}"),
    };
    assert_eq!(migration, BookmarkStoreMigration::None);
    assert_eq!(imported, expected);
    assert!(matches!(job, BookmarksIoJob::Done));
  }

  #[test]
  fn import_json_job_reports_failure() {
    let mut job = BookmarksIoJob::default();
    job.start_import_json("not valid json".to_string()).unwrap();

    let update = wait_for_update(&mut job);
    match update {
      BookmarksIoJobUpdate::ImportJsonFinished { result: Err(err) } => {
        assert!(err.contains("Failed to import bookmarks"));
      }
      other => panic!("expected import json error, got {other:?}"),
    }
    assert!(matches!(job, BookmarksIoJob::Error { .. }));
  }

  #[test]
  fn export_json_job_reports_disconnected_failure() {
    let (tx, rx) = mpsc::channel::<BookmarksIoJobUpdate>();
    drop(tx);

    let mut job = BookmarksIoJob::ExportingJson { rx };
    match job.poll() {
      Some(BookmarksIoJobUpdate::ExportJsonFinished { result: Err(err) }) => {
        assert!(err.contains("disconnected"));
      }
      other => panic!("expected disconnected JSON export error, got {other:?}"),
    }
    assert!(matches!(job, BookmarksIoJob::Error { .. }));
  }

  #[test]
  fn export_json_start_is_non_blocking_for_large_store() {
    use crate::ui::bookmarks::BookmarkEntry;
    use crate::ui::{BookmarkId, BookmarkNode};

    let mut store = BookmarkStore::default();
    let count: u64 = 20_000;
    // Make entries large enough that synchronous JSON pretty-printing would be
    // noticeable, while keeping overall test runtime reasonable.
    let long_title = "Synthetic Bookmark Title ".repeat(20);

    for i in 1..=count {
      let id = BookmarkId(i);
      let entry = BookmarkEntry {
        id,
        url: format!("https://example.com/{i}"),
        title: Some(long_title.clone()),
        added_at_ms: 0,
        parent: None,
      };
      store.roots.push(id);
      store.nodes.insert(id, BookmarkNode::Bookmark(entry));
    }
    store.next_id = BookmarkId(count + 1);

    let mut job = BookmarksIoJob::default();
    let start = Instant::now();
    job.start_export_json(store).unwrap();
    let elapsed = start.elapsed();
    assert!(
      elapsed < Duration::from_secs(1),
      "start_export_json should be fast; took {elapsed:?}"
    );
    assert!(job.is_exporting_json());

    // Ensure we don't leave a large background serialization thread running beyond this test.
    // (The important part is that *starting* the job is fast; completion can take longer.)
    let start_wait = Instant::now();
    loop {
      if let Some(update) = job.poll() {
        assert!(
          matches!(update, BookmarksIoJobUpdate::ExportJsonFinished { .. }),
          "expected ExportJsonFinished update, got {update:?}"
        );
        break;
      }
      assert!(
        start_wait.elapsed() < Duration::from_secs(15),
        "timed out waiting for large JSON export job to complete"
      );
      std::thread::sleep(Duration::from_millis(5));
    }
  }
}
