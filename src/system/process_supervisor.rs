//! Child process supervision utilities.
//!
//! This module provides small, reusable helpers for spawning and supervising child processes
//! (timeouts, termination escalation, stable exit status formatting). It is intended to be reused
//! by CLI tooling as well as the multiprocess browser (renderer/network processes).

use std::io;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Grace period between sending a graceful termination request and escalating to a forced kill.
///
/// Mirrors `timeout -k 10 ...` semantics.
pub const KILL_ESCALATION_GRACE_PERIOD: Duration = Duration::from_secs(10);

// After SIGKILL we still don't want to hang forever in pathological cases (e.g. uninterruptible
// sleep). Give the kernel a moment to schedule the process and reap it, then return an error.
const SIGKILL_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Lightweight summary of a process exit status (code + optional signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatusSummary {
  pub code: Option<i32>,
  pub signal: Option<i32>,
}

impl ExitStatusSummary {
  /// Create a summary from an [`ExitStatus`].
  pub fn from_exit_status(status: &ExitStatus) -> Self {
    summarize_exit_status(status)
  }

  /// Deterministically format the exit status summary.
  pub fn format(self) -> String {
    format_exit_status(self)
  }
}

impl std::fmt::Display for ExitStatusSummary {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&format_exit_status(*self))
  }
}

/// Extract a platform-agnostic summary from an [`ExitStatus`].
pub fn summarize_exit_status(status: &ExitStatus) -> ExitStatusSummary {
  ExitStatusSummary {
    code: status.code(),
    signal: {
      #[cfg(unix)]
      {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
      }
      #[cfg(not(unix))]
      {
        None
      }
    },
  }
}

/// Human-readable formatting for [`ExitStatusSummary`].
pub fn format_exit_status(status: ExitStatusSummary) -> String {
  match (status.code, status.signal) {
    (Some(code), Some(signal)) => format!("code {code} (signal {signal})"),
    (Some(code), None) => format!("code {code}"),
    (None, Some(signal)) => format!("signal {signal}"),
    (None, None) => "unknown status".to_string(),
  }
}

/// Wait for a child process to exit without blocking longer than `timeout`.
fn try_wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
  let deadline = Instant::now() + timeout;
  loop {
    if let Some(status) = child.try_wait()? {
      return Ok(Some(status));
    }
    let now = Instant::now();
    if now >= deadline {
      return Ok(None);
    }
    let remaining = deadline.duration_since(now);
    std::thread::sleep(remaining.min(Duration::from_millis(20)));
  }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> io::Result<()> {
  if pid == 0 || pid > i32::MAX as u32 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "child pid out of range",
    ));
  }

  // SAFETY: `kill` is a direct libc call with the required types.
  let result = unsafe { libc::kill(pid as i32, signal) };
  if result == 0 {
    return Ok(());
  }
  let err = io::Error::last_os_error();
  match err.raw_os_error() {
    Some(code) if code == libc::ESRCH => Ok(()),
    _ => Err(err),
  }
}

/// Attempt to terminate the child process gracefully, escalating to a forced kill.
///
/// On Unix this sends `SIGTERM`, waits [`KILL_ESCALATION_GRACE_PERIOD`], then sends `SIGKILL` if the
/// child is still alive. On non-Unix platforms we fall back to `Child::kill()` (best-effort).
///
/// This helper never blocks indefinitely. If the child still has not exited shortly after the
/// forced kill attempt, an `io::ErrorKind::TimedOut` error is returned.
pub fn kill_with_escalation(child: &mut Child) -> io::Result<ExitStatus> {
  if let Some(status) = child.try_wait()? {
    return Ok(status);
  }

  #[cfg(unix)]
  {
    let _ = send_signal(child.id(), libc::SIGTERM);
  }

  #[cfg(not(unix))]
  {
    let _ = child.kill();
  }

  if let Some(status) = try_wait_with_timeout(child, KILL_ESCALATION_GRACE_PERIOD)? {
    return Ok(status);
  }

  // Escalate.
  let _ = child.kill();

  if let Some(status) = try_wait_with_timeout(child, SIGKILL_WAIT_TIMEOUT)? {
    return Ok(status);
  }

  Err(io::Error::new(
    io::ErrorKind::TimedOut,
    "child did not exit after kill escalation",
  ))
}

/// Result for [`wait_with_timeout`].
#[derive(Debug)]
pub enum WaitWithTimeoutOutcome {
  /// The process exited within the timeout.
  Exited(ExitStatus),
  /// The timeout elapsed; the process was killed via [`kill_with_escalation`].
  TimedOut(ExitStatus),
}

/// Wait for a child process to exit, enforcing a wall-clock timeout.
///
/// When the timeout elapses, the child is terminated via [`kill_with_escalation`].
pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<WaitWithTimeoutOutcome> {
  if let Some(status) = try_wait_with_timeout(child, timeout)? {
    return Ok(WaitWithTimeoutOutcome::Exited(status));
  }

  let status = kill_with_escalation(child)?;
  Ok(WaitWithTimeoutOutcome::TimedOut(status))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::process::Command;

  #[test]
  fn exit_status_summary_formatting_is_deterministic() {
    let code_only = ExitStatusSummary {
      code: Some(7),
      signal: None,
    };
    assert_eq!(code_only.format(), "code 7");
    assert_eq!(code_only.to_string(), "code 7");

    let signal_only = ExitStatusSummary {
      code: None,
      signal: Some(9),
    };
    assert_eq!(signal_only.format(), "signal 9");
    assert_eq!(signal_only.to_string(), "signal 9");

    let both = ExitStatusSummary {
      code: Some(2),
      signal: Some(15),
    };
    assert_eq!(both.format(), "code 2 (signal 15)");
  }

  #[cfg(unix)]
  #[test]
  fn summarize_exit_status_reports_signal_for_sigkill() {
    let mut child = Command::new("sleep")
      .arg("1000")
      .spawn()
      .expect("spawn sleep");
    child.kill().expect("kill sleep");
    let status = child.wait().expect("wait sleep");
    let summary = summarize_exit_status(&status);
    assert_eq!(summary.code, None);
    assert_eq!(summary.signal, Some(libc::SIGKILL));
    assert_eq!(summary.to_string(), format!("signal {}", libc::SIGKILL));
  }

  #[cfg(unix)]
  #[test]
  fn kill_with_escalation_does_not_hang_when_sigterm_ignored() {
    let mut child = Command::new("sh")
      .arg("-c")
      .arg("trap '' TERM; exec sleep 1000")
      .spawn()
      .expect("spawn ignoring child");
    let start = Instant::now();
    let status = kill_with_escalation(&mut child).expect("kill with escalation");
    let elapsed = start.elapsed();
    assert!(
      elapsed < Duration::from_secs(20),
      "kill_with_escalation should not hang; took {elapsed:?}"
    );
    // The child ignores SIGTERM, so we should eventually SIGKILL it.
    let summary = summarize_exit_status(&status);
    assert_eq!(summary.signal, Some(libc::SIGKILL));
  }
}

