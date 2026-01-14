//! Background session autosave + crash marker plumbing.
//!
//! The windowed browser UI should never block its UI thread on disk I/O. `SessionAutosave` provides
//! a small helper that:
//! - Spawns a background writer thread.
//! - Coalesces `request_save` calls and writes only the latest snapshot (with a max write interval
//!   to avoid trailing-edge debounce starvation).
//! - Marks the on-disk session as "unclean" on startup (crash marker).
//! - Marks the on-disk session as "clean" on explicit shutdown (best-effort).
//!
//! This module is behind the `browser_ui` feature gate so core renderer builds remain lean.

use crate::ui::about_pages;
use crate::ui::session::{load_session, save_session_atomic, BrowserSession};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);
/// Maximum time the writer thread will wait before persisting a pending snapshot, even if new save
/// requests keep arriving within the debounce window.
///
/// Without this cap, "trailing edge" debounce can starve forever under continuous save traffic.
const MAX_WRITE_INTERVAL: Duration = Duration::from_secs(5);

type SaveSessionFn =
  Arc<dyn Fn(&Path, &BrowserSession) -> Result<(), String> + Send + Sync + 'static>;

/// Snapshot of the autosave worker's most recent write outcomes.
#[derive(Debug, Clone, Default)]
pub struct SessionAutosaveStatusSnapshot {
  /// Number of consecutive write failures since the last successful write.
  pub consecutive_failures: usize,
  /// Most recent write error (set on the first failure; updated with later failures).
  pub last_error: Option<String>,
  /// Timestamp of the first failure in the current failure streak.
  pub failed_since: Option<Instant>,
  /// Timestamp of the most recent write attempt (success or failure).
  pub last_attempt_at: Option<Instant>,
  /// Timestamp of the most recent successful write.
  pub last_success_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct SessionAutosaveStatusInner {
  consecutive_failures: usize,
  last_error: Option<String>,
  failed_since: Option<Instant>,
  last_attempt_at: Option<Instant>,
  last_success_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct SessionAutosaveStatusShared {
  revision: AtomicUsize,
  inner: Mutex<SessionAutosaveStatusInner>,
}

impl SessionAutosaveStatusShared {
  fn record_attempt(&self, result: &Result<(), String>, at: Instant) {
    let mut inner = self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    inner.last_attempt_at = Some(at);
    match result {
      Ok(()) => {
        inner.consecutive_failures = 0;
        inner.last_error = None;
        inner.failed_since = None;
        inner.last_success_at = Some(at);
      }
      Err(err) => {
        if inner.consecutive_failures == 0 {
          inner.failed_since = Some(at);
        }
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        inner.last_error = Some(err.clone());
      }
    }
    drop(inner);
    self.revision.fetch_add(1, Ordering::Release);
  }

  fn snapshot(&self) -> SessionAutosaveStatusSnapshot {
    let inner = self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    SessionAutosaveStatusSnapshot {
      consecutive_failures: inner.consecutive_failures,
      last_error: inner.last_error.clone(),
      failed_since: inner.failed_since,
      last_attempt_at: inner.last_attempt_at,
      last_success_at: inner.last_success_at,
    }
  }

  fn revision(&self) -> usize {
    self.revision.load(Ordering::Acquire)
  }
}

/// Non-blocking view of the session autosave writer health.
///
/// This handle is cheap to clone and intended to be held by UI components that want to surface
/// autosave failures to the user without blocking on disk I/O.
#[derive(Debug, Clone)]
pub struct SessionAutosaveStatusHandle {
  shared: Arc<SessionAutosaveStatusShared>,
}

impl SessionAutosaveStatusHandle {
  /// Read the latest autosave status (may block briefly on a mutex).
  pub fn snapshot(&self) -> SessionAutosaveStatusSnapshot {
    self.shared.snapshot()
  }

  /// Attempt to read the latest autosave status only when it has changed since `last_seen_revision`.
  ///
  /// This is non-blocking for the UI thread: if the writer thread is currently updating the status,
  /// `None` is returned and the caller can retry later.
  pub fn try_snapshot(
    &self,
    last_seen_revision: &mut usize,
  ) -> Option<SessionAutosaveStatusSnapshot> {
    let current_rev = self.shared.revision();
    if current_rev == *last_seen_revision {
      return None;
    }

    let inner = match self.shared.inner.try_lock() {
      Ok(guard) => guard,
      Err(_) => return None,
    };
    let snapshot = SessionAutosaveStatusSnapshot {
      consecutive_failures: inner.consecutive_failures,
      last_error: inner.last_error.clone(),
      failed_since: inner.failed_since,
      last_attempt_at: inner.last_attempt_at,
      last_success_at: inner.last_success_at,
    };
    drop(inner);

    *last_seen_revision = self.shared.revision();
    Some(snapshot)
  }
}

/// UI-facing policy/state for deciding when to surface session-autosave failures.
///
/// This is kept UI-framework-agnostic (no egui types) so it can be unit tested and reused by other
/// frontends.
#[derive(Debug, Default)]
pub struct SessionAutosaveWarningUiState {
  warning_visible: bool,
  warning_dismissed: bool,
  warning_text: Option<String>,
  last_error: Option<String>,
  last_warning_shown_at: Option<Instant>,
  last_resumed_toast_at: Option<Instant>,
  warned_for_current_failure_streak: bool,
  was_failing: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionAutosaveWarningUiUpdate {
  /// A warning just became visible (transitioned from hidden to visible).
  pub show_warning: bool,
  /// The warning just cleared (failure → success).
  pub cleared_warning: bool,
  /// Session writes recovered after a warning was shown; an optional "resumed" toast can be shown.
  pub show_resumed_toast: bool,
}

impl SessionAutosaveWarningUiState {
  const WARN_AFTER_CONSECUTIVE_FAILURES: usize = 2;
  const WARN_AFTER_DURATION: Duration = Duration::from_secs(5);
  const WARNING_COOLDOWN: Duration = Duration::from_secs(30);
  const RESUMED_TOAST_COOLDOWN: Duration = Duration::from_secs(15);
  const MAX_ERROR_CHARS: usize = 500;

  pub fn warning_text(&self) -> Option<&str> {
    if self.warning_visible {
      self.warning_text.as_deref()
    } else {
      None
    }
  }

  pub fn warning_visible(&self) -> bool {
    self.warning_visible
  }

  pub fn dismiss(&mut self) {
    self.warning_visible = false;
    self.warning_dismissed = true;
  }

  /// Advance the state machine using the latest autosave status.
  pub fn update(
    &mut self,
    status: &SessionAutosaveStatusSnapshot,
    now: Instant,
  ) -> SessionAutosaveWarningUiUpdate {
    let is_failing = status.consecutive_failures > 0;
    let mut out = SessionAutosaveWarningUiUpdate::default();

    if is_failing {
      self.was_failing = true;

      let error = status
        .last_error
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown error");
      let mut error_short = error
        .chars()
        .take(Self::MAX_ERROR_CHARS)
        .collect::<String>();
      if error.chars().count() > Self::MAX_ERROR_CHARS {
        error_short.push('…');
      }

      if Some(error) != self.last_error.as_deref() {
        self.last_error = Some(error.to_string());
        self.warning_text = Some(format!("Failed to save session: {error_short}"));
      }

      let failures_threshold_met =
        status.consecutive_failures >= Self::WARN_AFTER_CONSECUTIVE_FAILURES;
      let duration_threshold_met = status
        .failed_since
        .is_some_and(|t| now.saturating_duration_since(t) >= Self::WARN_AFTER_DURATION);
      let should_warn =
        (failures_threshold_met || duration_threshold_met) && !self.warning_dismissed;

      if should_warn && !self.warning_visible {
        let cooled_down = self
          .last_warning_shown_at
          .is_none_or(|t| now.saturating_duration_since(t) >= Self::WARNING_COOLDOWN);
        if cooled_down {
          self.warning_visible = true;
          self.last_warning_shown_at = Some(now);
          self.warned_for_current_failure_streak = true;
          out.show_warning = true;
        }
      }

      return out;
    }

    // Not failing: clear any existing warning/dismissal state.
    if self.was_failing {
      out.cleared_warning = self.warning_visible || self.warning_dismissed;
      if self.warned_for_current_failure_streak {
        let cooled_down = self
          .last_resumed_toast_at
          .is_none_or(|t| now.saturating_duration_since(t) >= Self::RESUMED_TOAST_COOLDOWN);
        if cooled_down {
          out.show_resumed_toast = true;
          self.last_resumed_toast_at = Some(now);
        }
      }
    }

    self.warning_visible = false;
    self.warning_dismissed = false;
    self.warning_text = None;
    self.last_error = None;
    self.warned_for_current_failure_streak = false;
    self.was_failing = false;
    out
  }
}

enum Command {
  Save(BrowserSession),
  Flush(mpsc::Sender<Result<(), String>>),
  Shutdown(mpsc::Sender<Result<(), String>>),
}

<<<<<<< HEAD
trait ThreadSpawner {
  fn spawn<F>(&self, name: String, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
  where
    F: FnOnce() + Send + 'static;
}

struct StdThreadSpawner;

impl ThreadSpawner for StdThreadSpawner {
  fn spawn<F>(&self, name: String, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
  where
    F: FnOnce() + Send + 'static,
  {
    std::thread::Builder::new().name(name).spawn(f)
=======
#[derive(Debug)]
struct SyncFallbackState {
  current_session: BrowserSession,
  pending_session: Option<BrowserSession>,
  last_write_result: Result<(), String>,
}

/// Synchronous fallback path used when spawning the writer thread fails.
///
/// This keeps `SessionAutosave` functional (no silent data loss) at the cost of doing disk I/O on
/// the caller's thread.
struct SyncFallback {
  path: PathBuf,
  save_fn: SaveSessionFn,
  write_count: Arc<AtomicUsize>,
  status: Arc<SessionAutosaveStatusShared>,
  state: Mutex<SyncFallbackState>,
}

impl SyncFallback {
  fn request_save(&self, session: BrowserSession) {
    let mut state = self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut to_write = session;
    to_write.did_exit_cleanly = false;
    // Preserve the crash-loop streak tracked at startup.
    to_write.unclean_exit_streak = state.current_session.unclean_exit_streak;

    let result = (self.save_fn)(self.path.as_path(), &to_write);
    self.status.record_attempt(&result, Instant::now());
    state.last_write_result = result;

    if state.last_write_result.is_ok() {
      self.write_count.fetch_add(1, Ordering::Relaxed);
      state.pending_session = None;
      state.current_session = to_write;
    } else {
      state.pending_session = Some(to_write);
    }
  }

  fn flush(&self) -> Result<(), String> {
    let mut state = self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(mut session) = state.pending_session.take() {
      session.did_exit_cleanly = false;
      session.unclean_exit_streak = state.current_session.unclean_exit_streak;
      let result = (self.save_fn)(self.path.as_path(), &session);
      self.status.record_attempt(&result, Instant::now());
      state.last_write_result = result;
      if state.last_write_result.is_ok() {
        self.write_count.fetch_add(1, Ordering::Relaxed);
        state.current_session = session;
      } else {
        state.pending_session = Some(session);
      }
      return state.last_write_result.clone();
    }

    if state.last_write_result.is_err() {
      state.current_session.did_exit_cleanly = false;
      let result = (self.save_fn)(self.path.as_path(), &state.current_session);
      self.status.record_attempt(&result, Instant::now());
      state.last_write_result = result;
      if state.last_write_result.is_ok() {
        self.write_count.fetch_add(1, Ordering::Relaxed);
      }
    }

    state.last_write_result.clone()
  }

  fn shutdown(self) -> Result<(), String> {
    let mut state = self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut to_write = state
      .pending_session
      .take()
      .unwrap_or_else(|| state.current_session.clone());
    to_write.did_exit_cleanly = true;
    to_write.unclean_exit_streak = 0;

    let result = (self.save_fn)(self.path.as_path(), &to_write);
    self.status.record_attempt(&result, Instant::now());
    state.last_write_result = result.clone();
    if result.is_ok() {
      self.write_count.fetch_add(1, Ordering::Relaxed);
      state.current_session = to_write;
    } else {
      state.pending_session = Some(to_write);
    }

    result
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
  }
}

/// Background session autosave worker.
///
/// This type is intended to be owned by the UI thread. Disk I/O happens on a dedicated background
/// thread so callers can "schedule" saves without blocking.
pub struct SessionAutosave {
  path: PathBuf,
  tx: Option<mpsc::Sender<Command>>,
  join: Option<std::thread::JoinHandle<()>>,
  sync_fallback: Option<SyncFallback>,
  write_count: Arc<AtomicUsize>,
  status: SessionAutosaveStatusHandle,
  save_fn: SaveSessionFn,
  worker_running: AtomicBool,
  spawn_error: Option<String>,
}

impl SessionAutosave {
  /// Spawn a background writer thread and immediately (best-effort) write a crash marker by setting
  /// `did_exit_cleanly=false` in the on-disk session.
  pub fn new(path: PathBuf) -> Self {
    Self::new_with_debounce_and_initial(path, DEFAULT_DEBOUNCE, None)
  }

  /// Spawn a background writer thread that immediately (best-effort) writes the provided initial
  /// session snapshot with `did_exit_cleanly=false`.
  ///
  /// This is intended for startup crash marking: callers can restore/build a session snapshot in
  /// memory first (including any window state) and have the autosave worker persist that exact
  /// snapshot as soon as it starts.
  ///
  /// If the on-disk session file exists but cannot be read/parsed (e.g. JSON corruption), the
  /// autosave worker will *not* overwrite it on startup. The file is preserved until the first
  /// explicit [`Self::request_save`] call.
  pub fn new_with_initial_session(path: PathBuf, initial_session: BrowserSession) -> Self {
    Self::new_with_debounce_and_initial(path, DEFAULT_DEBOUNCE, Some(initial_session))
  }

  fn new_with_debounce(path: PathBuf, debounce: Duration) -> Self {
    Self::new_with_debounce_and_initial(path, debounce, None)
  }

  fn new_with_debounce_and_initial(
    path: PathBuf,
    debounce: Duration,
    initial_session: Option<BrowserSession>,
  ) -> Self {
<<<<<<< HEAD
    Self::new_with_debounce_and_initial_and_max_interval(
      path,
      debounce,
      MAX_WRITE_INTERVAL,
      initial_session,
    )
  }

  fn new_with_debounce_and_initial_and_max_interval(
    path: PathBuf,
    debounce: Duration,
    max_write_interval: Duration,
    initial_session: Option<BrowserSession>,
  ) -> Self {
    let save_fn: SaveSessionFn = Arc::new(|path, session| save_session_atomic(path, session));
    Self::new_with_debounce_and_initial_and_max_interval_with_spawner_and_saver(
      path,
      debounce,
      max_write_interval,
      initial_session,
      save_fn,
      &StdThreadSpawner,
    )
  }

  #[cfg(test)]
  fn new_with_debounce_and_saver(path: PathBuf, debounce: Duration, save_fn: SaveSessionFn) -> Self {
    Self::new_with_debounce_and_initial_and_max_interval_with_spawner_and_saver(
      path,
      debounce,
      MAX_WRITE_INTERVAL,
      None,
      save_fn,
      &StdThreadSpawner,
    )
  }

  #[cfg(test)]
  fn new_with_debounce_and_initial_with_spawner<S: ThreadSpawner>(
    path: PathBuf,
    debounce: Duration,
    initial_session: Option<BrowserSession>,
    spawner: &S,
  ) -> Self {
    let save_fn: SaveSessionFn = Arc::new(|path, session| save_session_atomic(path, session));
    Self::new_with_debounce_and_initial_and_max_interval_with_spawner_and_saver(
      path,
      debounce,
      MAX_WRITE_INTERVAL,
      initial_session,
      save_fn,
      spawner,
    )
  }

  fn new_with_debounce_and_initial_and_max_interval_with_spawner_and_saver<S: ThreadSpawner>(
    path: PathBuf,
    debounce: Duration,
    max_write_interval: Duration,
    initial_session: Option<BrowserSession>,
    save_fn: SaveSessionFn,
    spawner: &S,
  ) -> Self {
    let path_for_struct = path.clone();
=======
    let save_fn: SaveSessionFn = Arc::new(|path, session| save_session_atomic(path, session));
    Self::new_with_debounce_and_initial_and_saver(path, debounce, initial_session, save_fn)
  }

  fn new_with_debounce_and_initial_and_saver(
    path: PathBuf,
    debounce: Duration,
    initial_session: Option<BrowserSession>,
    save_fn: SaveSessionFn,
  ) -> Self {
    Self::new_with_debounce_and_initial_and_saver_and_spawner(
      path,
      debounce,
      initial_session,
      save_fn,
      |thread_main| {
        std::thread::Builder::new()
          .name("browser_session_autosave".to_string())
          .spawn(thread_main)
      },
    )
  }

  #[cfg(test)]
  fn new_with_debounce_and_saver(
    path: PathBuf,
    debounce: Duration,
    save_fn: SaveSessionFn,
  ) -> Self {
    Self::new_with_debounce_and_initial_and_saver(path, debounce, None, save_fn)
  }

  #[cfg(test)]
  fn new_with_debounce_and_saver_forced_spawn_failure(
    path: PathBuf,
    debounce: Duration,
    save_fn: SaveSessionFn,
  ) -> Self {
    Self::new_with_debounce_and_initial_and_saver_and_spawner(
      path,
      debounce,
      None,
      save_fn,
      |_thread_main| {
        Err(std::io::Error::new(
          std::io::ErrorKind::Other,
          "forced spawn failure (test)",
        ))
      },
    )
  }

  fn new_with_debounce_and_initial_and_saver_and_spawner(
    path: PathBuf,
    debounce: Duration,
    initial_session: Option<BrowserSession>,
    save_fn: SaveSessionFn,
    spawner: impl FnOnce(
      Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<std::thread::JoinHandle<()>>,
  ) -> Self {
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
    let (tx, rx) = mpsc::channel::<Command>();
    let write_count = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(SessionAutosaveStatusShared::default());
    let status_handle = SessionAutosaveStatusHandle {
      shared: Arc::clone(&status),
    };

<<<<<<< HEAD
    // Preserve the initial snapshot for a synchronous fallback in case thread spawning fails.
    let initial_session_cell = Arc::new(Mutex::new(initial_session));

    let join = spawner.spawn("browser_session_autosave".to_string(), {
      let write_count = Arc::clone(&write_count);
      let status = Arc::clone(&status);
      let save_fn = Arc::clone(&save_fn);
      let path_for_thread = path;
      let initial_session_cell = Arc::clone(&initial_session_cell);
      move || {
        let initial_session = initial_session_cell
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .take();
        session_writer_thread(
          path_for_thread,
          debounce,
          max_write_interval,
          initial_session,
          rx,
          write_count,
          status,
          save_fn,
        )
      }
    });

    match join {
      Ok(join) => Self {
        path: path_for_struct,
        tx: Some(tx),
        join: Some(join),
        write_count,
        status: status_handle,
        save_fn,
        worker_running: AtomicBool::new(true),
        spawn_error: None,
      },
      Err(err) => {
        drop(tx);
        let initial_session = initial_session_cell
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .take();
        let _ = session_startup_unclean_marker(
          &path_for_struct,
          initial_session,
          &write_count,
          status.as_ref(),
=======
    let thread_main: Box<dyn FnOnce() + Send + 'static> = Box::new({
      let path = path.clone();
      let initial_session = initial_session.clone();
      let write_count = Arc::clone(&write_count);
      let status = Arc::clone(&status);
      let save_fn = Arc::clone(&save_fn);
      move || {
        session_writer_thread(
          path,
          debounce,
          initial_session,
          rx,
          write_count,
          status,
          save_fn,
        )
      }
    });

    match spawner(thread_main) {
      Ok(join) => Self {
        tx: Some(tx),
        join: Some(join),
        sync_fallback: None,
        write_count,
        status: status_handle,
      },
      Err(err) => {
        eprintln!(
          "failed to spawn session autosave writer thread ({err}); falling back to synchronous session saves"
        );

        let (current_session, last_write_result) = startup_mark_unclean(
          path.as_path(),
          initial_session,
          &write_count,
          &status,
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
          &save_fn,
        );

        Self {
<<<<<<< HEAD
          path: path_for_struct,
          tx: None,
          join: None,
          write_count,
          status: status_handle,
          save_fn,
          worker_running: AtomicBool::new(false),
          spawn_error: Some(format!("failed to spawn session autosave thread: {err}")),
=======
          tx: None,
          join: None,
          sync_fallback: Some(SyncFallback {
            path,
            save_fn,
            write_count: Arc::clone(&write_count),
            status: Arc::clone(&status),
            state: Mutex::new(SyncFallbackState {
              current_session,
              pending_session: None,
              last_write_result,
            }),
          }),
          write_count,
          status: status_handle,
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
        }
      }
    }
  }

  /// Whether the background autosave writer thread is running.
  ///
  /// If this returns `false`, callers should fall back to synchronous `save_session_atomic` writes
  /// rather than silently dropping autosave requests.
  pub fn is_background_thread_running(&self) -> bool {
    self.worker_running.load(Ordering::Acquire) && self.tx.is_some()
  }

  /// If the background writer thread failed to spawn, return the error message.
  pub fn spawn_error(&self) -> Option<&str> {
    self.spawn_error.as_deref()
  }

  /// Schedule saving the latest session snapshot.
  ///
  /// This call is non-blocking; it simply sends the snapshot to the writer thread. Multiple rapid
  /// calls are debounced/coalesced so only the latest snapshot is persisted.
  pub fn request_save(&self, session: BrowserSession) {
<<<<<<< HEAD
    let mut session = session;
    // Running sessions should always be persisted as "unclean". The clean marker is only written
    // on explicit shutdown.
    session.did_exit_cleanly = false;

    if self.is_background_thread_running() {
      if let Some(tx) = self.tx.as_ref() {
        match tx.send(Command::Save(session)) {
          Ok(()) => return,
          Err(mpsc::SendError(Command::Save(session))) => {
            self.disable_worker("session autosave thread disconnected");
            self.save_sync(session);
            return;
          }
          Err(mpsc::SendError(_)) => unreachable!("request_save only sends Save commands"),
        }
      }
      self.disable_worker("session autosave thread is not running");
    }

    self.save_sync(session);
=======
    if let Some(sync) = self.sync_fallback.as_ref() {
      sync.request_save(session);
      return;
    }
    let Some(tx) = self.tx.as_ref() else {
      return;
    };
    let _ = tx.send(Command::Save(session));
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
  }

  /// Block until the currently queued snapshot (if any) has been written.
  pub fn flush(&self, timeout: Duration) -> Result<(), String> {
    if let Some(sync) = self.sync_fallback.as_ref() {
      let _ = timeout;
      return sync.flush();
    }
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
    if let Some(sync) = self.sync_fallback.take() {
      let _ = timeout;
      self.tx = None;
      self.join = None;
      return sync.shutdown();
    }

    let deadline = Instant::now() + timeout;
    let Some(tx) = self.tx.take() else {
      self.worker_running.store(false, Ordering::Release);
      return self.mark_session_clean_sync();
    };
    self.worker_running.store(false, Ordering::Release);

    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    if tx.send(Command::Shutdown(done_tx)).is_err() {
      // Thread is already dead: best-effort mark whatever is on disk as clean, but still return an
      // error so callers can fall back to persisting their in-memory snapshot.
      let _ = self.mark_session_clean_sync();
      return Err("session autosave thread disconnected".to_string());
    }

    let save_result = match done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
      Ok(result) => result,
      Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
        "timed out after {timeout:?} waiting for session autosave shutdown save"
      )),
      Err(mpsc::RecvTimeoutError::Disconnected) => Err(
        "session autosave thread disconnected while waiting for shutdown acknowledgement"
          .to_string(),
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
        Ok(Err(_)) => {
          eprintln!("session autosave thread panicked during shutdown");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
          eprintln!(
            "timed out after {timeout:?} waiting for session autosave thread to exit; shutting down anyway"
          );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          eprintln!("session autosave join helper thread disconnected during shutdown");
        }
      }
    }

    save_result
  }

  /// Most recent session write error observed by the autosave worker thread.
  ///
  /// This is updated whenever an on-disk write fails and cleared on the next successful write.
  pub fn last_error(&self) -> Option<String> {
    self.status.snapshot().last_error
  }

  fn disable_worker(&self, reason: &str) {
    if self
      .worker_running
      .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      eprintln!("{reason}; falling back to synchronous session saves");
    }
  }

  fn save_sync(&self, session: BrowserSession) {
    let mut session = session;
    // Session autosaves always represent a running browser, so ensure the crash marker stays set.
    session.did_exit_cleanly = false;

    // Preserve the crash-loop streak managed by the autosave worker. UI snapshots do not track it.
    if let Ok(Some(existing)) = load_session(&self.path) {
      session.unclean_exit_streak = existing.unclean_exit_streak;
    }

    let result = (self.save_fn)(self.path.as_path(), &session);
    self.status.shared.record_attempt(&result, Instant::now());
    if result.is_ok() {
      self.write_count.fetch_add(1, Ordering::Relaxed);
    }
  }

  fn mark_session_clean_sync(&self) -> Result<(), String> {
    match load_session(&self.path)? {
      Some(mut session) => {
        session.did_exit_cleanly = true;
        session.unclean_exit_streak = 0;
        let result = (self.save_fn)(self.path.as_path(), &session);
        self.status.shared.record_attempt(&result, Instant::now());
        if result.is_ok() {
          self.write_count.fetch_add(1, Ordering::Relaxed);
        }
        result
      }
      None => Ok(()),
    }
  }

  #[cfg(test)]
  fn successful_write_count(&self) -> usize {
    self.write_count.load(Ordering::Relaxed)
  }

  pub fn status_handle(&self) -> SessionAutosaveStatusHandle {
    self.status.clone()
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

fn startup_mark_unclean(
  path: &Path,
  initial_session: Option<BrowserSession>,
  write_count: &AtomicUsize,
  status: &SessionAutosaveStatusShared,
  save_fn: &SaveSessionFn,
) -> (BrowserSession, Result<(), String>) {
  match initial_session {
    Some(mut session) => {
      // Crash-loop tracking: bump the unclean-exit streak when we mark the session as running.
      //
      // When an initial in-memory snapshot is supplied, it does not contain the previous-run crash
      // marker. Best-effort read the on-disk marker to determine whether the previous run exited
      // cleanly.
      let (prev_clean, prev_streak) = match load_session(path) {
        Ok(Some(prev)) => (prev.did_exit_cleanly, prev.unclean_exit_streak),
        Ok(None) => (true, 0),
        Err(_) => (true, 0),
      };
      session.unclean_exit_streak = if prev_clean {
        1
      } else {
        prev_streak.saturating_add(1)
      };

      session.did_exit_cleanly = false;
      let result = save_fn(path, &session);
      status.record_attempt(&result, Instant::now());
      if result.is_ok() {
        write_count.fetch_add(1, Ordering::Relaxed);
      }
      (session, result)
    }
    None => match load_session(path) {
      Ok(Some(mut session)) => {
        session.unclean_exit_streak = if session.did_exit_cleanly {
          1
        } else {
          session.unclean_exit_streak.saturating_add(1)
        };
        session.did_exit_cleanly = false;
        let result = save_fn(path, &session);
        status.record_attempt(&result, Instant::now());
        if result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        (session, result)
      }
      Ok(None) => {
        let mut session = BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string());
        session.did_exit_cleanly = false;
        session.unclean_exit_streak = 1;
        let result = save_fn(path, &session);
        status.record_attempt(&result, Instant::now());
        if result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        (session, result)
      }
      Err(_) => {
        let mut session = BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string());
        session.did_exit_cleanly = false;
        session.unclean_exit_streak = 1;
        // Leave `last_write_result` as Ok so `flush()` doesn't try to "repair" the file by writing
        // our fallback session.
        (session, Ok(()))
      }
    },
  }
}

fn session_writer_thread(
  path: PathBuf,
  debounce: Duration,
  max_write_interval: Duration,
  initial_session: Option<BrowserSession>,
  rx: mpsc::Receiver<Command>,
  write_count: Arc<AtomicUsize>,
  status: Arc<SessionAutosaveStatusShared>,
  save_fn: SaveSessionFn,
) {
<<<<<<< HEAD
  let (mut current_session, mut last_write_result) = session_startup_unclean_marker(
    &path,
    initial_session,
    &write_count,
    status.as_ref(),
    &save_fn,
  );

  // (session, updated_at, first_pending_at)
  let mut pending: Option<(BrowserSession, Instant, Instant)> = None;
=======
  // On startup: best-effort mark the on-disk session as "unclean" so crash recovery can detect
  // abnormal exits.
  //
  // If an initial in-memory snapshot is provided, prefer that over whatever happens to be on disk.
  // This ensures early crashes (before the UI's first autosave tick) still leave a correct session
  // snapshot for recovery.
  //
  // If the on-disk session cannot be parsed, do **not** overwrite it with a blank default session:
  // leave the file untouched and wait for the first explicit Save request from the UI.
  let (mut current_session, mut last_write_result) = startup_mark_unclean(
    path.as_path(),
    initial_session,
    &write_count,
    &status,
    &save_fn,
  );

  let mut pending: Option<(BrowserSession, Instant)> = None;
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)

  loop {
    // If there's a pending snapshot and either the debounce window elapsed or we've exceeded the
    // maximum write interval, persist it now.
    if let Some((session, updated_at, first_pending_at)) = pending.take() {
      let now = Instant::now();
      if now.saturating_duration_since(updated_at) >= debounce
        || now.saturating_duration_since(first_pending_at) >= max_write_interval
      {
        let mut to_write = session;
        to_write.did_exit_cleanly = false;
        // Preserve the crash-loop streak managed by this thread. Session snapshots produced by the
        // UI do not track it.
        to_write.unclean_exit_streak = current_session.unclean_exit_streak;
        last_write_result = save_fn(path.as_path(), &to_write);
        status.record_attempt(&last_write_result, Instant::now());
        if last_write_result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
          current_session = to_write;
        } else {
          let retry_at = Instant::now();
          pending = Some((to_write, retry_at, retry_at));
        }
        continue;
      } else {
        pending = Some((session, updated_at, first_pending_at));
      }
    }

    let recv_result = if let Some((_, updated_at, first_pending_at)) = pending.as_ref() {
      let debounce_deadline = *updated_at + debounce;
      let forced_deadline = *first_pending_at + max_write_interval;
      let deadline = std::cmp::min(debounce_deadline, forced_deadline);
      let timeout = deadline.saturating_duration_since(Instant::now());
      rx.recv_timeout(timeout)
    } else {
      rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
    };

    match recv_result {
      Ok(Command::Save(session)) => {
        let now = Instant::now();
        match pending.as_mut() {
          Some((pending_session, updated_at, _first_pending_at)) => {
            *pending_session = session;
            *updated_at = now;
          }
          None => {
            pending = Some((session, now, now));
          }
        }
      }
      Ok(Command::Flush(done_tx)) => {
        let result = if let Some((session, _, _)) = pending.take() {
          let mut to_write = session;
          to_write.did_exit_cleanly = false;
          to_write.unclean_exit_streak = current_session.unclean_exit_streak;
          last_write_result = save_fn(path.as_path(), &to_write);
          status.record_attempt(&last_write_result, Instant::now());
          if last_write_result.is_ok() {
            write_count.fetch_add(1, Ordering::Relaxed);
            current_session = to_write;
          } else {
            let retry_at = Instant::now();
            pending = Some((to_write, retry_at, retry_at));
          }
          last_write_result.clone()
        } else {
          // If the last write failed (e.g. due to a transient filesystem issue), allow `flush()` to
          // retry persisting the current session even when there is no pending update.
          if last_write_result.is_err() {
            current_session.did_exit_cleanly = false;
            last_write_result = save_fn(path.as_path(), &current_session);
            status.record_attempt(&last_write_result, Instant::now());
            if last_write_result.is_ok() {
              write_count.fetch_add(1, Ordering::Relaxed);
            }
          }
          last_write_result.clone()
        };
        let _ = done_tx.send(result);
      }
      Ok(Command::Shutdown(done_tx)) => {
        let mut to_write = pending
          .take()
<<<<<<< HEAD
          .map(|(session, _, _)| session)
=======
          .map(|(session, _)| session)
>>>>>>> 70796210 (fix(ui): fall back to sync session autosave when thread spawn fails)
          .unwrap_or(current_session);
        to_write.did_exit_cleanly = true;
        to_write.unclean_exit_streak = 0;
        last_write_result = save_fn(path.as_path(), &to_write);
        status.record_attempt(&last_write_result, Instant::now());
        if last_write_result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        let _ = done_tx.send(last_write_result.clone());
        return;
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        // Debounce/max-write interval elapsed; loop will persist the pending session.
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        // No more senders: best-effort flush any pending snapshot (as unclean) and exit.
        if let Some((session, _, _)) = pending.take() {
          let mut to_write = session;
          to_write.did_exit_cleanly = false;
          to_write.unclean_exit_streak = current_session.unclean_exit_streak;
          let result = save_fn(path.as_path(), &to_write);
          status.record_attempt(&result, Instant::now());
          if result.is_ok() {
            write_count.fetch_add(1, Ordering::Relaxed);
          }
        }
        return;
      }
    }
  }
}

fn session_startup_unclean_marker(
  path: &Path,
  initial_session: Option<BrowserSession>,
  write_count: &Arc<AtomicUsize>,
  status: &SessionAutosaveStatusShared,
  save_fn: &SaveSessionFn,
) -> (BrowserSession, Result<(), String>) {
  // On startup: best-effort mark the on-disk session as "unclean" so crash recovery can detect
  // abnormal exits.
  //
  // If an initial in-memory snapshot is provided, prefer that over whatever happens to be on disk.
  // This ensures early crashes (before the UI's first autosave tick) still leave a correct session
  // snapshot for recovery.
  //
  // If the on-disk session cannot be parsed, do **not** overwrite it with a blank default session:
  // leave the file untouched and wait for the first explicit Save request from the UI.
  match initial_session {
    Some(mut session) => {
      // Crash-loop tracking: bump the unclean-exit streak when we mark the session as running.
      //
      // When an initial in-memory snapshot is supplied, it does not contain the previous-run crash
      // marker. Best-effort read the on-disk marker to determine whether the previous run exited
      // cleanly.
      let (prev_clean, prev_streak, can_overwrite_on_startup) = match load_session(path) {
        Ok(Some(prev)) => (prev.did_exit_cleanly, prev.unclean_exit_streak, true),
        Ok(None) => (true, 0, true),
        // If the session file exists but can't be read/parsed (e.g. JSON corruption), preserve it
        // until the UI makes an explicit save request.
        Err(_) => (true, 0, false),
      };
      session.unclean_exit_streak = if prev_clean {
        1
      } else {
        prev_streak.saturating_add(1)
      };
      session.did_exit_cleanly = false;

      if !can_overwrite_on_startup {
        // Leave `last_write_result` as Ok so `flush()` doesn't try to "repair" the file by writing
        // our fallback session.
        (session, Ok(()))
      } else {
        let result = save_fn(path, &session);
        status.record_attempt(&result, Instant::now());
        if result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        (session, result)
      }
    }
    None => match load_session(path) {
      Ok(Some(mut session)) => {
        session.unclean_exit_streak = if session.did_exit_cleanly {
          1
        } else {
          session.unclean_exit_streak.saturating_add(1)
        };
        session.did_exit_cleanly = false;
        let result = save_fn(path, &session);
        status.record_attempt(&result, Instant::now());
        if result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        (session, result)
      }
      Ok(None) => {
        let mut session = BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string());
        session.did_exit_cleanly = false;
        session.unclean_exit_streak = 1;
        let result = save_fn(path, &session);
        status.record_attempt(&result, Instant::now());
        if result.is_ok() {
          write_count.fetch_add(1, Ordering::Relaxed);
        }
        (session, result)
      }
      Err(_) => {
        let mut session = BrowserSession::single(about_pages::ABOUT_NEWTAB.to_string());
        session.did_exit_cleanly = false;
        session.unclean_exit_streak = 1;
        // Leave `last_write_result` as Ok so `flush()` doesn't try to "repair" the file by writing
        // our fallback session.
        (session, Ok(()))
      }
    },
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;

  #[test]
  fn startup_writes_provided_initial_snapshot_as_unclean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    assert!(!path.exists());

    let mut initial = BrowserSession::single("about:blank".to_string());
    initial.home_url = "about:blank".to_string();
    initial.did_exit_cleanly = true;

    let autosave = SessionAutosave::new_with_debounce_and_initial(
      path.clone(),
      Duration::from_millis(10),
      Some(initial.clone()),
    );
    autosave.flush(Duration::from_secs(2)).unwrap();

    let mut expected = initial.sanitized();
    expected.did_exit_cleanly = false;
    expected.unclean_exit_streak = 1;

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session, expected);
  }

  #[test]
  fn startup_increments_unclean_exit_streak_with_initial_snapshot_when_previous_exit_was_unclean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let mut prev = BrowserSession::single("about:blank".to_string());
    prev.did_exit_cleanly = false;
    prev.unclean_exit_streak = 2;
    save_session_atomic(&path, &prev).unwrap();

    let initial = BrowserSession::single("about:error".to_string());
    let autosave = SessionAutosave::new_with_debounce_and_initial(
      path.clone(),
      Duration::from_millis(10),
      Some(initial.clone()),
    );
    autosave.flush(Duration::from_secs(2)).unwrap();

    let mut expected = initial.sanitized();
    expected.did_exit_cleanly = false;
    expected.unclean_exit_streak = 3;

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session, expected);
  }

  #[test]
  fn startup_creates_minimal_session_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    assert!(!path.exists());

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(10));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.version, 2);
    assert!(!session.did_exit_cleanly);
    assert_eq!(session.unclean_exit_streak, 1);
    assert_eq!(session.windows.len(), 1);
    assert_eq!(session.windows[0].tabs.len(), 1);
    assert_eq!(session.windows[0].tabs[0].url, about_pages::ABOUT_NEWTAB);
    assert_eq!(session.windows[0].active_tab_index, 0);
  }

  #[test]
  fn spawn_failure_falls_back_to_synchronous_saves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    assert!(!path.exists());

    let save_fn: SaveSessionFn = Arc::new(|path, session| save_session_atomic(path, session));
    let autosave = SessionAutosave::new_with_debounce_and_saver_forced_spawn_failure(
      path.clone(),
      Duration::from_millis(10),
      save_fn,
    );

    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.windows[0].tabs[0].url, "about:blank");
    assert!(!session.did_exit_cleanly);
    assert_eq!(session.unclean_exit_streak, 1);
  }

  #[test]
  fn debounce_persists_latest_snapshot() {
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
      session.windows[0].tabs[0].url, "about:error",
      "expected only the final snapshot to be persisted"
    );
    assert!(
      !session.did_exit_cleanly,
      "expected running sessions to be unclean"
    );
    assert_eq!(session.unclean_exit_streak, 1);
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
    assert_eq!(session.unclean_exit_streak, 1);
    assert_eq!(autosave.successful_write_count(), baseline + 1);
  }

  #[test]
  fn max_write_interval_prevents_trailing_edge_debounce_starvation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    // Send Save requests more frequently than the debounce window. Without the max-write-interval
    // cap this would starve forever (trailing-edge debounce never expires).
    let debounce = Duration::from_millis(200);
    let max_write_interval = Duration::from_millis(300);
    let tick = Duration::from_millis(50);

    let autosave = SessionAutosave::new_with_debounce_and_initial_and_max_interval(
      path.clone(),
      debounce,
      max_write_interval,
      None,
    );
    autosave.flush(Duration::from_secs(2)).unwrap();

    let baseline = autosave.successful_write_count();

    // Phase 1: spam non-final snapshots for longer than max_write_interval.
    let start = Instant::now();
    while start.elapsed() < max_write_interval.saturating_mul(2) {
      autosave.request_save(BrowserSession::single("about:phase1".to_string()));
      std::thread::sleep(tick);
    }

    // Phase 2: keep spamming the final snapshot until we observe it written, without requiring the
    // sender to become idle (i.e. without waiting for the trailing-edge debounce window to expire).
    //
    // Use unique URLs so we can assert the forced write persisted the most recent snapshot we had
    // sent at the time we observed the write.
    let mut writes_seen = autosave.successful_write_count();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed_final_write = false;
    let mut observed_url = None::<String>;
    let mut last_sent_url = None::<String>;
    let mut i = 0usize;
    while Instant::now() < deadline {
      let current_writes = autosave.successful_write_count();
      if current_writes > writes_seen {
        writes_seen = current_writes;
        if let Some(last_url) = last_sent_url.as_deref() {
          let session = load_session(&path).unwrap().unwrap();
          if session.windows[0].tabs[0].url == last_url {
            observed_final_write = true;
            observed_url = Some(last_url.to_string());
            break;
          }
        }
      }

      let url = format!("about:final#{i}");
      last_sent_url = Some(url.clone());
      autosave.request_save(BrowserSession::single(url));
      i = i.saturating_add(1);
      std::thread::sleep(tick);
    }

    assert!(
      observed_final_write,
      "expected at least one write of the latest snapshot even while Save requests keep arriving within the debounce window; baseline={baseline}, after={}",
      autosave.successful_write_count()
    );

    let observed_url = observed_url.expect("expected observed_url to be set when observed_final_write=true");
    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.windows.len(), 1);
    assert_eq!(
      session.windows[0].tabs[0].url, observed_url,
      "expected the max-write-interval flush to persist the latest snapshot"
    );
    assert!(
      !session.did_exit_cleanly,
      "expected running sessions to be marked unclean"
    );
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
    assert!(
      !session.did_exit_cleanly,
      "startup should mark session as unclean"
    );
    assert_eq!(session.unclean_exit_streak, 1);

    autosave.shutdown(Duration::from_secs(2)).unwrap();
    let session = load_session(&path).unwrap().unwrap();
    assert!(
      session.did_exit_cleanly,
      "clean shutdown should mark session as clean"
    );
    assert_eq!(session.unclean_exit_streak, 0);
  }

  #[test]
  fn drop_does_not_mark_session_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    {
      let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(10));
      autosave.request_save(BrowserSession::single("about:blank".to_string()));
      autosave.flush(Duration::from_secs(2)).unwrap();
      // Drop without calling `shutdown()`: should *not* mark the session as clean.
    }

    let session = load_session(&path).unwrap().unwrap();
    assert!(
      !session.did_exit_cleanly,
      "dropping SessionAutosave should not mark the session as clean"
    );
    assert_eq!(session.unclean_exit_streak, 1);
  }

  #[test]
  fn startup_increments_unclean_exit_streak_when_previous_exit_was_unclean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let mut initial = BrowserSession::single("about:blank".to_string());
    initial.did_exit_cleanly = false;
    initial.unclean_exit_streak = 2;
    save_session_atomic(&path, &initial).unwrap();

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(10));
    autosave.flush(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert!(!session.did_exit_cleanly);
    assert_eq!(session.unclean_exit_streak, 3);
  }

  #[test]
  fn startup_does_not_overwrite_invalid_json_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let corrupted = "this is not valid JSON\n";
    std::fs::write(&path, corrupted).unwrap();

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(10));
    // Allow the writer thread to run its startup logic; `flush()` should be a no-op in this case.
    autosave.flush(Duration::from_secs(2)).unwrap();

    // The corrupted file should be preserved until a real Save request is made.
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, corrupted);
  }

  #[test]
  fn startup_does_not_overwrite_invalid_json_session_file_with_initial_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let corrupted = "this is not valid JSON\n";
    std::fs::write(&path, corrupted).unwrap();

    let initial = BrowserSession::single("about:blank".to_string());
    let autosave = SessionAutosave::new_with_debounce_and_initial(
      path.clone(),
      Duration::from_millis(10),
      Some(initial),
    );
    // Allow the writer thread to run its startup logic; `flush()` should be a no-op in this case.
    autosave.flush(Duration::from_secs(2)).unwrap();

    // The corrupted file should be preserved until a real Save request is made.
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, corrupted);
  }

  #[test]
  fn records_error_state_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    // Corrupt the on-disk session so the writer thread doesn't attempt a startup write; we want the
    // test-controlled save attempt below to be the first write.
    std::fs::write(&path, "not valid json\n").unwrap();

    let save_fn: SaveSessionFn = Arc::new(|_path, _session| Err("disk full".to_string()));
    let autosave =
      SessionAutosave::new_with_debounce_and_saver(path, Duration::from_millis(10), save_fn);

    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    assert!(autosave.flush(Duration::from_secs(2)).is_err());

    let status = autosave.status_handle().snapshot();
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(status.last_error.as_deref(), Some("disk full"));
    assert!(status.failed_since.is_some());
    assert!(status.last_attempt_at.is_some());
    assert!(status.last_success_at.is_none());
  }

  #[test]
  fn successful_write_clears_error_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    std::fs::write(&path, "not valid json\n").unwrap();

    let attempts = Arc::new(AtomicUsize::new(0));
    let save_fn: SaveSessionFn = Arc::new({
      let attempts = Arc::clone(&attempts);
      move |_path, _session| {
        let n = attempts.fetch_add(1, Ordering::Relaxed);
        if n < 2 {
          Err(format!("fail {n}"))
        } else {
          Ok(())
        }
      }
    });
    let autosave =
      SessionAutosave::new_with_debounce_and_saver(path, Duration::from_millis(10), save_fn);

    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    assert!(autosave.flush(Duration::from_secs(2)).is_err());
    let status = autosave.status_handle().snapshot();
    assert_eq!(status.consecutive_failures, 1);
    assert!(status
      .last_error
      .as_deref()
      .unwrap_or_default()
      .contains("fail"));

    autosave.request_save(BrowserSession::single("about:newtab".to_string()));
    assert!(autosave.flush(Duration::from_secs(2)).is_err());
    let status = autosave.status_handle().snapshot();
    assert_eq!(status.consecutive_failures, 2);
    assert!(status
      .last_error
      .as_deref()
      .unwrap_or_default()
      .contains("fail"));

    autosave.request_save(BrowserSession::single("about:error".to_string()));
    autosave.flush(Duration::from_secs(2)).unwrap();
    let status = autosave.status_handle().snapshot();
    assert_eq!(status.consecutive_failures, 0);
    assert!(status.last_error.is_none());
    assert!(status.failed_since.is_none());
    assert!(status.last_success_at.is_some());
  }

  #[test]
  fn autosave_warning_ui_state_threshold_and_dismissal() {
    let mut ui = SessionAutosaveWarningUiState::default();
    let start = Instant::now();

    let mut failing = SessionAutosaveStatusSnapshot::default();
    failing.consecutive_failures = 1;
    failing.last_error = Some("disk full".to_string());
    failing.failed_since = Some(start);
    failing.last_attempt_at = Some(start);

    let update = ui.update(&failing, start);
    assert!(!update.show_warning);
    assert!(!ui.warning_visible());

    failing.consecutive_failures = 2;
    failing.last_attempt_at = Some(start + Duration::from_secs(1));
    let update = ui.update(&failing, start + Duration::from_secs(1));
    assert!(
      update.show_warning,
      "expected warning after repeated failures"
    );
    assert!(ui.warning_visible());

    ui.dismiss();
    assert!(!ui.warning_visible());

    // Further failures in the same streak should not re-open the warning once dismissed.
    let update = ui.update(&failing, start + Duration::from_secs(2));
    assert!(!update.show_warning);
    assert!(!ui.warning_visible());

    let recovered = SessionAutosaveStatusSnapshot::default();
    let update = ui.update(&recovered, start + Duration::from_secs(3));
    assert!(update.cleared_warning);
    assert!(update.show_resumed_toast);

    // Rate limiting: a new failure streak shortly after the prior warning should not immediately
    // re-warn.
    let mut failing_again = SessionAutosaveStatusSnapshot::default();
    failing_again.consecutive_failures = 2;
    failing_again.last_error = Some("disk full".to_string());
    failing_again.failed_since = Some(start + Duration::from_secs(4));
    let update = ui.update(&failing_again, start + Duration::from_secs(4));
    assert!(
      !update.show_warning,
      "expected warning cooldown to suppress spam"
    );
  }

  #[test]
  fn last_error_is_populated_when_writes_fail() {
    let dir = tempfile::tempdir().unwrap();
    // Portable "unwritable file" trick: give the autosave worker a directory path instead of a
    // file path, so `persist()` fails.
    let path = dir.path().join("session_dir");
    std::fs::create_dir(&path).unwrap();

    let autosave = SessionAutosave::new_with_debounce(path.clone(), Duration::from_millis(10));
    autosave.request_save(BrowserSession::single("about:blank".to_string()));
    let _ = autosave.flush(Duration::from_secs(2));
    assert!(
      autosave.last_error().is_some(),
      "expected autosave to record a write error for directory path {}",
      path.display()
    );
  }

  struct FailingThreadSpawner;

  impl ThreadSpawner for FailingThreadSpawner {
    fn spawn<F>(&self, _name: String, _f: F) -> std::io::Result<std::thread::JoinHandle<()>>
    where
      F: FnOnce() + Send + 'static,
    {
      Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "simulated thread spawn failure",
      ))
    }
  }

  #[test]
  fn spawn_failure_falls_back_to_synchronous_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");

    let autosave = SessionAutosave::new_with_debounce_and_initial_with_spawner(
      path.clone(),
      Duration::from_millis(10),
      None,
      &FailingThreadSpawner,
    );
    assert!(
      !autosave.is_background_thread_running(),
      "expected autosave worker to be disabled"
    );
    assert!(
      autosave.spawn_error().is_some(),
      "expected thread spawn error to be exposed"
    );

    autosave.request_save(BrowserSession::single("about:blank".to_string()));

    // `request_save` should persist synchronously when the background thread is unavailable.
    let session = load_session(&path).unwrap().unwrap();
    assert_eq!(session.windows[0].tabs[0].url, "about:blank");
    assert!(!session.did_exit_cleanly);
    assert_eq!(session.unclean_exit_streak, 1);

    let mut autosave = autosave;
    autosave.shutdown(Duration::from_secs(2)).unwrap();

    let session = load_session(&path).unwrap().unwrap();
    assert!(session.did_exit_cleanly, "expected shutdown to mark session clean");
    assert_eq!(session.unclean_exit_streak, 0);
  }
} 
