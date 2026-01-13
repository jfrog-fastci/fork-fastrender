use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Tracks detached thread joins during shutdown so the UI thread never blocks on `JoinHandle::join`.
///
/// Pattern:
/// - The UI thread drops the channels/senders that tell a worker thread to exit.
/// - It then hands the `JoinHandle` to `ShutdownJoinTracker::track_join`, which spawns a tiny
///   join-helper thread and returns immediately.
/// - The event loop periodically calls `poll()` to observe completion and log failures/timeouts.
/// - If the join does not complete within `detach_timeout`, the tracker stops tracking it (the
///   join-helper thread remains detached and will finish whenever the worker does).
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
  rx: mpsc::Receiver<std::thread::Result<()>>,
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

  /// Spawns a detached join-helper thread and begins tracking completion for logging/cleanup.
  ///
  /// This method must be *non-blocking*; it should be safe to call on the UI/event-loop thread.
  pub fn track_join(&mut self, label: impl Into<String>, join: JoinHandle<()>) {
    self.track_blocking(label, move || join.join());
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
          rx: done_rx,
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
    let mut remaining: Vec<TrackedJoin> = Vec::with_capacity(self.joins.len());

    for entry in self.joins.drain(..) {
      match entry.rx.try_recv() {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
          let elapsed = now.saturating_duration_since(entry.started_at);
          eprintln!("shutdown join observed panic: {} (after {:?})", entry.label, elapsed);
        }
        Err(mpsc::TryRecvError::Empty) => {
          if now >= entry.deadline {
            eprintln!(
              "shutdown join timed out after {:?}: {} (detaching)",
              self.detach_timeout, entry.label
            );
          } else {
            remaining.push(entry);
          }
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          eprintln!("shutdown join helper thread disconnected: {}", entry.label);
        }
      }
    }

    self.joins = remaining;
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
  use std::time::{Duration, Instant};

  #[test]
  fn poll_is_non_blocking_and_times_out() {
    let mut tracker = ShutdownJoinTracker::with_detach_timeout(Duration::from_millis(50));

    let join = std::thread::spawn(|| {
      std::thread::sleep(Duration::from_millis(200));
    });

    tracker.track_join("test-sleeper", join);

    // `poll` should not block even though the join-helper is still waiting on the thread.
    let start = Instant::now();
    tracker.poll();
    assert!(start.elapsed() < Duration::from_millis(100));

    // Wait past the detach timeout, then poll again; it should stop tracking.
    std::thread::sleep(Duration::from_millis(80));
    tracker.poll();
    assert!(!tracker.has_pending(), "expected join to be detached after timeout");
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
