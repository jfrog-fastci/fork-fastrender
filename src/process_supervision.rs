//! Small utilities for supervising child processes (timeouts, deterministic kill, exit status
//! summaries).
//!
//! This is intentionally lightweight so both CLI tools and future multiprocess browser code can
//! reuse the same hard-timeout and kill/reap logic.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

pub use crate::cli_utils::render_pipeline::{format_exit_status, summarize_exit_status, ExitStatusSummary};

/// Default time limit for waiting on a killed child to exit.
///
/// The goal is to avoid hanging forever in pathological situations (e.g. a child stuck in
/// uninterruptible sleep), while still waiting long enough for normal OS scheduling to reap the
/// process deterministically in the common case.
pub const DEFAULT_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Wraps a running [`std::process::Child`] along with the [`Instant`] it was started.
#[derive(Debug)]
pub struct RunningChild {
  child: Child,
  started: Instant,
}

/// Results from [`RunningChild::kill_and_wait`].
#[derive(Debug)]
pub struct KillAndWaitOutcome {
  pub kill_result: io::Result<()>,
  pub wait_result: io::Result<ExitStatus>,
}

impl RunningChild {
  /// Wrap an already-spawned [`Child`].
  pub fn new(child: Child) -> Self {
    Self {
      child,
      started: Instant::now(),
    }
  }

  /// Elapsed time since the child was started.
  pub fn elapsed(&self) -> Duration {
    self.started.elapsed()
  }

  /// Returns true when the hard timeout has elapsed.
  pub fn hard_timed_out(&self, hard_timeout: Duration) -> bool {
    self.elapsed() >= hard_timeout
  }

  /// Non-blocking wait for the child to exit.
  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    self.child.try_wait()
  }

  /// Send a hard kill to the child and wait for it to exit (with a bounded timeout).
  ///
  /// This method is designed to be deterministic and non-hanging:
  /// - Always attempts to reap the child after sending a kill.
  /// - Never blocks longer than [`DEFAULT_KILL_WAIT_TIMEOUT`] waiting for the child to exit.
  ///
  /// If reaping times out, a background thread continues waiting so the process can still be
  /// collected when/if it eventually exits.
  pub fn kill_and_wait(self) -> KillAndWaitOutcome {
    self.kill_and_wait_timeout(DEFAULT_KILL_WAIT_TIMEOUT)
  }

  /// Variant of [`RunningChild::kill_and_wait`] with an explicit wait timeout.
  pub fn kill_and_wait_timeout(mut self, wait_timeout: Duration) -> KillAndWaitOutcome {
    let kill_result = self.child.kill();
    let wait_result = wait_child_with_timeout(self.child, wait_timeout);
    KillAndWaitOutcome {
      kill_result,
      wait_result,
    }
  }
}

fn wait_child_with_timeout(mut child: Child, timeout: Duration) -> io::Result<ExitStatus> {
  // We avoid calling `child.wait()` on the current thread so the hard-timeout path can't hang the
  // supervisor loop.
  let (tx, rx) = channel();
  std::thread::spawn(move || {
    let result = child.wait();
    let _ = tx.send(result);
  });

  match rx.recv_timeout(timeout) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
      io::ErrorKind::TimedOut,
      "timed out waiting for child to exit after kill",
    )),
    Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
      io::ErrorKind::Other,
      "child wait thread disconnected",
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::process::{Command, Stdio};
  use std::time::{Duration, Instant};

  #[test]
  fn exit_status_formatting() {
    assert_eq!(
      format_exit_status(ExitStatusSummary {
        code: Some(1),
        signal: None
      }),
      "code 1"
    );
    assert_eq!(
      format_exit_status(ExitStatusSummary {
        code: None,
        signal: Some(9)
      }),
      "signal 9"
    );
    assert_eq!(
      format_exit_status(ExitStatusSummary {
        code: Some(1),
        signal: Some(9)
      }),
      "code 1 (signal 9)"
    );
  }

  #[test]
  #[cfg(target_os = "linux")]
  fn kill_and_wait_terminates_child_within_deadline() {
    // `sleep` is available on Linux CI/agent environments and provides a stable long-running
    // process we can kill.
    let child = Command::new("sleep")
      .arg("10")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn sleep");
    let running = RunningChild::new(child);

    let started = Instant::now();
    let outcome = running.kill_and_wait_timeout(Duration::from_secs(1));
    let elapsed = started.elapsed();
    assert!(
      elapsed < Duration::from_secs(2),
      "kill_and_wait took too long: {elapsed:?}"
    );

    let status = outcome.wait_result.expect("wait after kill should succeed");
    assert!(
      !status.success(),
      "killed process unexpectedly reported success"
    );
  }
}
