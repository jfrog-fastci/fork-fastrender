//! Background session autosave + crash marker plumbing.
//!
//! The windowed browser UI should never block its UI thread on disk I/O. `SessionAutosave` provides
//! a small helper that:
//! - Spawns a background writer thread.
//! - Debounces `request_save` calls and writes only the latest snapshot.
//! - Marks the on-disk session as "unclean" on startup (crash marker).
//! - Marks the on-disk session as "clean" on explicit shutdown (best-effort).
//!
//! This module is behind the `browser_ui` feature gate so core renderer builds remain lean.

use crate::ui::about_pages;
use crate::ui::session::{load_session, save_session_atomic, BrowserSession};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

enum Command {
  Save(BrowserSession),
  Flush(mpsc::Sender<Result<(), String>>),
  Shutdown(mpsc::Sender<Result<(), String>>),
}

/// Background session autosave worker.
///
/// This type is intended to be owned by the UI thread. Disk I/O happens on a dedicated background
/// thread so callers can "schedule" saves without blocking.
pub struct SessionAutosave {
  tx: Option<mpsc::Sender<Command>>,
  join: Option<std::thread::JoinHandle<()>>,
  write_count: Arc<AtomicUsize>,
}

impl SessionAutosave {
  /// Spawn a background writer thread and immediately (best-effort) write a crash marker by setting
  /// `did_exit_cleanly=false` in the on-disk session.
  pub fn new(path: PathBuf) -> Self {
    Self::new_with_debounce(path, DEFAULT_DEBOUNCE)
  }

  fn new_with_debounce(path: PathBuf, debounce: Duration) -> Self {
    let (tx, rx) = mpsc::channel::<Command>();
    let write_count = Arc::new(AtomicUsize::new(0));

    let join = std::thread::Builder::new()
      .name("browser_session_autosave".to_string())
      .spawn({
        let write_count = Arc::clone(&write_count);
        move || session_writer_thread(path, debounce, rx, write_count)
      })
      .ok();

    Self {
      tx: Some(tx),
      join,
      write_count,
    }
  }

  /// Schedule saving the latest session snapshot.
  ///
  /// This call is non-blocking; it simply sends the snapshot to the writer thread. Multiple rapid
  /// calls are debounced/coalesced so only the latest snapshot is persisted.
  pub fn request_save(&self, session: BrowserSession) {
    let Some(tx) = self.tx.as_ref() else {
      return;
    };
    let _ = tx.send(Command::Save(session));
  }

  /// Block until the currently queued snapshot (if any) has been written.
  pub fn flush(&self, timeout: Duration) -> Result<(), String> {
    let Some(tx) = self.tx.as_ref() else {
      return Err("session autosave thread is not running".to_string());
    };

    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    tx.send(Command::Flush(done_tx))
      .map_err(|_| "session autosave thread disconnected".to_string())?;

    match done_rx.recv_timeout(timeout) {
      Ok(result) => result,
      Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
        "timed out after {timeout:?} waiting for session autosave flush"
      )),
      Err(mpsc::RecvTimeoutError::Disconnected) => Err(
        "session autosave thread disconnected while waiting for flush acknowledgement".to_string(),
      ),
    }
  }

  /// Mark the on-disk session as clean (`did_exit_cleanly=true`), flush, and stop the writer thread.
  ///
  /// Best-effort: on timeout the writer thread may continue running in the background.
  pub fn shutdown(&mut self, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let Some(tx) = self.tx.take() else {
      return Ok(());
    };

    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    tx.send(Command::Shutdown(done_tx))
      .map_err(|_| "session autosave thread disconnected".to_string())?;

    let save_result = match done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
      Ok(result) => result,
      Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
        "timed out after {timeout:?} waiting for session autosave shutdown save"
      )),
      Err(mpsc::RecvTimeoutError::Disconnected) => Err(
        "session autosave thread disconnected while waiting for shutdown acknowledgement".to_string(),
      ),
    };

    if let Some(join) = self.join.take() {
      // `JoinHandle` has no timeout API. Mirror the browser binary's pattern: join on a helper
      // thread and wait on a channel.
      let (join_tx, join_rx) = mpsc::channel::<std::thread::Result<()>>();
      let _ = std::thread::spawn(move || {
        let _ = join_tx.send(join.join());
      });
      match join_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("session autosave thread panicked".to_string()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
          return Err(format!(
            "timed out after {timeout:?} waiting for session autosave thread to exit"
          ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          return Err("session autosave join helper thread disconnected".to_string());
        }
      }
    }

    save_result
  }

  #[cfg(test)]
  fn successful_write_count(&self) -> usize {
    self.write_count.load(Ordering::Relaxed)
  }
}

impl Drop for SessionAutosave {
  fn drop(&mut self) {
    // Intentionally do *not* mark the session as clean here.
    //
    // Crash recovery relies on the on-disk marker (`did_exit_cleanly=false`) remaining set unless
    // the caller explicitly requests a clean shutdown.
    let Some(tx) = self.tx.take() else {
      return;
    };
    drop(tx);
    // Don't block drop on the join: dropping the JoinHandle detaches the thread. The writer thread
    // will notice the channel disconnect and exit promptly on its own.
  }
}

fn session_writer_thread(
  path: PathBuf,
  debounce: Duration,
  rx: mpsc::Receiver<Command>,
  write_count: Arc<AtomicUsize>,
) {
  // On startup: best-effort mark the on-disk session as "unclean" so crash recovery can detect
  // abnormal exits.
  let mut current_session = match load_session(&path) {
    Ok(Some(session)) => session,
    Ok(None) => BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string()),
    Err(_) => BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string()),
  };
  current_session.did_exit_cleanly = false;
  let mut last_write_result = save_session_atomic(&path, &current_session);
  if last_write_result.is_ok() {
    write_count.fetch_add(1, Ordering::Relaxed);
  }

  let mut pending: Option<(BrowserSession, Instant)> = None;

  loop {
    // If there's a pending snapshot and the debounce window elapsed, persist it now.
    if let Some((session, updated_at)) = pending.take() {
      if Instant::now().duration_since(updated_at) >= debounce {
        let mut to_write = session;
        to_write.did_exit_cleanly = false;
        last_write_result = save_session_atomic(&path, &to_write);
        if last_write_result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
          current_session = to_write;
        } else {
          pending = Some((to_write, Instant::now()));
        }
        continue;
      } else {
        pending = Some((session, updated_at));
      }
    }

    let recv_result = if let Some((_, updated_at)) = pending.as_ref() {
      let deadline = *updated_at + debounce;
      let timeout = deadline.saturating_duration_since(Instant::now());
      rx.recv_timeout(timeout)
    } else {
      rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
    };

    match recv_result {
      Ok(Command::Save(session)) => {
        pending = Some((session, Instant::now()));
      }
      Ok(Command::Flush(done_tx)) => {
        let result = if let Some((session, _)) = pending.take() {
          let mut to_write = session;
          to_write.did_exit_cleanly = false;
          last_write_result = save_session_atomic(&path, &to_write);
          if last_write_result.is_ok() {
            write_count.fetch_add(1, Ordering::Relaxed);
            current_session = to_write;
          } else {
            pending = Some((to_write, Instant::now()));
          }
          last_write_result.clone()
        } else {
          last_write_result.clone()
        };
        let _ = done_tx.send(result);
      }
      Ok(Command::Shutdown(done_tx)) => {
        let mut to_write = pending.take().map(|(session, _)| session).unwrap_or(current_session);
        to_write.did_exit_cleanly = true;
        last_write_result = save_session_atomic(&path, &to_write);
        if last_write_result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        let _ = done_tx.send(last_write_result.clone());
        return;
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        // Debounce elapsed; loop will persist the pending session.
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        // No more senders: best-effort flush any pending snapshot (as unclean) and exit.
        if let Some((session, _)) = pending.take() {
          let mut to_write = session;
          to_write.did_exit_cleanly = false;
          if save_session_atomic(&path, &to_write).is_ok() {
            write_count.fetch_add(1, Ordering::Relaxed);
          }
        }
        return;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn debounce_coalesces_to_single_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(40));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let baseline = autosave.successful_write_count();

    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    autosave.request_save(BrowserSession::single("about:newtab".to_string()));
    autosave.request_save(BrowserSession::single("about:error".to_string()));

    std::thread::sleep(Duration::from_millis(100));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.windows.len(), 1);
    assert_eq!(
      session.windows[0].tabs[0].url,
      "about:error",
      "expected only the final snapshot to be persisted"
    );
    assert!(!session.did_exit_cleanly, "expected running sessions to be unclean");
    assert_eq!(
      autosave.successful_write_count(),
      baseline + 1,
      "expected multiple request_save calls to coalesce into a single write"
    );
  }

  #[test]
  fn flush_writes_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_secs(60));
    autosave.flush(Duration::from_secs(2)).unwrap();
    let baseline = autosave.successful_write_count();

    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.windows.len(), 1);
    assert_eq!(session.windows[0].tabs[0].url, "about:blank");
    assert!(!session.did_exit_cleanly);
    assert_eq!(autosave.successful_write_count(), baseline + 1);
  }

  #[test]
  fn crash_marker_toggles_unclean_then_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let mut initial = BrowserSession::single("about:blank".to_string());
    initial.did_exit_cleanly = true;
    save_session_atomic(&path, &initial).unwrap();

    let mut autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(20));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert!(!session.did_exit_cleanly, "startup should mark session as unclean");

    autosave.shutdown(Duration::from_secs(2)).unwrap();
    let session = load_session(&path).unwrap().unwrap();
    assert!(session.did_exit_cleanly, "clean shutdown should mark session as clean");
  }
}
