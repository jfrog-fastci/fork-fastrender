use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Tracks detached thread joins during shutdown so the UI thread never blocks on `JoinHandle::join`.
///
/// Pattern:
/// - The UI thread drops the channels/senders that tell a worker thread to exit.
/// - It then hands join handles / blocking joins to `ShutdownJoinTracker`, which must return
///   immediately (non-blocking).
/// - The event loop periodically calls `poll()` to observe completion and log failures/timeouts.
/// - If a join does not complete within `detach_timeout`, the tracker stops tracking it so the UI
///   thread never blocks indefinitely.
///   - For `track_join` this also drops (detaches) the original `JoinHandle`.
///   - For `track_blocking` the detached helper thread may continue running; the tracker just stops
///     waiting/logging.
#[derive(Debug)]
pub struct ShutdownJoinTracker {
  detach_timeout: Duration,
  joins: Vec<TrackedJoin>,
}

#[derive(Debug)]
struct TrackedJoin {
  label: String,
  started_at: Instant,
  deadline: Instant,
  state: TrackedJoinState,
}

#[derive(Debug)]
enum TrackedJoinState {
  /// Join a real `JoinHandle` by polling `is_finished` from the UI thread.
  ///
  /// This avoids spawning an extra join-helper thread (and allows dropping the join handle after the
  /// timeout).
  JoinHandle(Option<JoinHandle<()>>),
  /// Observe completion of a potentially blocking operation via a detached helper thread.
  Blocking {
    rx: mpsc::Receiver<std::thread::Result<()>>,
  },
}

impl ShutdownJoinTracker {
  /// Default "give up tracking" timeout.
  ///
  /// This is intentionally longer than the old (blocking) 500ms join timeout, because we are no
  /// longer stalling the UI thread while waiting.
  const DEFAULT_DETACH_TIMEOUT: Duration = Duration::from_secs(5);

  pub fn new() -> Self {
    Self::with_detach_timeout(Self::DEFAULT_DETACH_TIMEOUT)
  }

  pub fn with_detach_timeout(detach_timeout: Duration) -> Self {
    Self {
      detach_timeout,
      joins: Vec::new(),
    }
  }

  /// Track a thread join without blocking the UI thread.
  ///
  /// The join handle is stored and polled via `JoinHandle::is_finished` inside `poll()`. When the
  /// thread completes, `poll()` joins it to surface panics.
  pub fn track_join(&mut self, label: impl Into<String>, join: JoinHandle<()>) {
    let started_at = Instant::now();
    let deadline = started_at + self.detach_timeout;
    self.joins.push(TrackedJoin {
      label: label.into(),
      started_at,
      deadline,
      state: TrackedJoinState::JoinHandle(Some(join)),
    });
  }

  /// Track a potentially blocking shutdown operation (e.g. joining a worker hidden behind an
  /// abstraction).
  ///
  /// The `join` closure is executed on a detached helper thread; the UI thread returns immediately.
  pub fn track_blocking<F>(&mut self, label: impl Into<String>, join: F)
  where
    F: FnOnce() -> std::thread::Result<()> + Send + 'static,
  {
    let label = label.into();
    let (done_tx, done_rx) = mpsc::channel::<std::thread::Result<()>>();
    // `spawn` takes ownership of the closure even on error, so move a clone into the helper thread
    // and keep the original `Sender` in this stack frame for error-path cleanup.
    let done_tx_thread = done_tx.clone();
    let started_at = Instant::now();
    let deadline = started_at + self.detach_timeout;

    // Best-effort. If we can't spawn, drop the `JoinHandle` (detach) and move on.
    match std::thread::Builder::new()
      .name(format!("fastr_shutdown_join_{}", self.joins.len()))
      .spawn(move || {
        let _ = done_tx_thread.send(join());
      }) {
      Ok(helper_join) => {
        // Detach the helper; `poll()` will observe completion via the channel.
        drop(helper_join);
        self.joins.push(TrackedJoin {
          label,
          started_at,
          deadline,
          state: TrackedJoinState::Blocking { rx: done_rx },
        });
      }
      Err(err) => {
        eprintln!("failed to spawn shutdown join helper thread for {label}: {err}");
        drop(done_rx);
      }
    }
  }

  /// Polls all tracked joins without blocking.
  pub fn poll(&mut self) {
    if self.joins.is_empty() {
      return;
    }

    let now = Instant::now();
    let detach_timeout = self.detach_timeout;
    self.joins.retain_mut(|entry| match &mut entry.state {
      TrackedJoinState::JoinHandle(join) => {
        let Some(handle) = join.as_ref() else {
          return false;
        };

        if handle.is_finished() {
          let handle = join.take().expect("checked Some above");
          match handle.join() {
            Ok(()) => {}
            Err(_) => {
              let elapsed = now.saturating_duration_since(entry.started_at);
              eprintln!(
                "shutdown join observed panic: {} (after {:?})",
                entry.label, elapsed
              );
            }
          }
          return false;
        }

        if now >= entry.deadline {
          eprintln!(
            "shutdown join timed out after {:?}: {} (detaching)",
            detach_timeout, entry.label
          );
          return false;
        }

        true
      }
      TrackedJoinState::Blocking { rx } => match rx.try_recv() {
        Ok(Ok(())) => false,
        Ok(Err(_)) => {
          let elapsed = now.saturating_duration_since(entry.started_at);
          eprintln!(
            "shutdown join observed panic: {} (after {:?})",
            entry.label, elapsed
          );
          false
        }
        Err(mpsc::TryRecvError::Empty) => {
          if now >= entry.deadline {
            eprintln!(
              "shutdown join timed out after {:?}: {} (detaching)",
              detach_timeout, entry.label
            );
            false
          } else {
            true
          }
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          eprintln!("shutdown join helper thread disconnected: {}", entry.label);
          false
        }
      },
    });
  }

  pub fn has_pending(&self) -> bool {
    !self.joins.is_empty()
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.joins.iter().map(|entry| entry.deadline).min()
  }
}

impl Default for ShutdownJoinTracker {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::ShutdownJoinTracker;
  use std::sync::mpsc;
  use std::time::{Duration, Instant};

  #[test]
  fn poll_is_non_blocking_and_times_out() {
    let mut tracker = ShutdownJoinTracker::with_detach_timeout(Duration::from_millis(50));

    // Use a channel so we can let the thread exit promptly even though the `JoinHandle` will be
    // detached after the timeout.
    let (tx, rx) = mpsc::channel::<()>();
    let join = std::thread::spawn(move || {
      let _ = rx.recv();
    });

    tracker.track_join("test-sleeper", join);

    // `poll` should not block even though the join-helper is still waiting on the thread.
    let start = Instant::now();
    tracker.poll();
    assert!(start.elapsed() < Duration::from_millis(100));

    // Wait past the detach timeout, then poll again; it should stop tracking.
    std::thread::sleep(Duration::from_millis(80));
    tracker.poll();
    assert!(
      !tracker.has_pending(),
      "expected join to be detached after timeout"
    );

    // Allow the thread to exit so the test harness doesn't keep extra threads around.
    let _ = tx.send(());
    std::thread::sleep(Duration::from_millis(5));
  }

  #[test]
  fn track_join_returns_quickly_for_slow_threads() {
    let mut tracker = ShutdownJoinTracker::with_detach_timeout(Duration::from_secs(5));

    let join = std::thread::spawn(|| {
      std::thread::sleep(Duration::from_millis(1050));
    });

    let start = Instant::now();
    tracker.track_join("slow-thread", join);
    assert!(
      start.elapsed() < Duration::from_millis(100),
      "tracking a join must be non-blocking"
    );

    // Ensure the join eventually completes so the test doesn't leave a long-running thread behind.
    std::thread::sleep(Duration::from_millis(1150));
    tracker.poll();
    assert!(!tracker.has_pending());
  }
}
