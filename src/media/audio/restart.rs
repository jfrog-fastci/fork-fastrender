use std::time::{Duration, Instant};

/// Factory for opening a "default output" audio stream.
///
/// This trait exists to make the restart logic testable without depending on CPAL. A production
/// implementation can:
/// - query the current default device/config
/// - build and `play()` an output stream
/// - hook the stream error callback to the owning backend
pub(crate) trait AudioStreamFactory {
  type Stream;
  type Error: std::fmt::Debug;

  /// Attempt to open (and start) the default output stream.
  fn open_default_stream(&mut self) -> Result<Self::Stream, Self::Error>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RestartPolicy {
  /// Maximum number of consecutive open failures tolerated before falling back to silence/null.
  pub max_attempts: usize,
  /// Backoff duration after the first failure (subsequent failures are exponential).
  pub initial_backoff: Duration,
  /// Maximum backoff duration between attempts.
  pub max_backoff: Duration,
}

impl RestartPolicy {
  fn backoff_for_failure(&self, failures: usize) -> Duration {
    if failures == 0 {
      return Duration::ZERO;
    }
    // failures=1 => initial_backoff * 1
    // failures=2 => initial_backoff * 2
    // failures=3 => initial_backoff * 4
    let shift = failures.saturating_sub(1).min(30);
    let base_ms = u64::try_from(self.initial_backoff.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(self.max_backoff.as_millis()).unwrap_or(u64::MAX);
    let factor = 1u64.checked_shl(shift as u32).unwrap_or(u64::MAX);
    let scaled = base_ms.saturating_mul(factor);
    Duration::from_millis(scaled.min(max_ms))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartState {
  /// Stream is alive and no restart is pending.
  Running,
  /// We are waiting for a scheduled time before attempting to re-open the stream.
  Restarting {
    failures: usize,
    next_attempt_at: Instant,
  },
  /// Restart attempts exhausted; audio output is permanently silenced.
  Fallback,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickOutcome {
  pub opened_stream: bool,
  pub entered_fallback: bool,
}

/// A small, deterministic restart state machine.
///
/// The machine owns the currently-open stream (if any). When `request_restart` is called, the
/// machine drops the current stream and schedules a re-open attempt. Callers drive it by calling
/// [`ResilientStreamManager::tick`] with a supplied `now`.
pub(crate) struct ResilientStreamManager<P: AudioStreamFactory> {
  policy: RestartPolicy,
  provider: P,
  state: RestartState,
  stream: Option<P::Stream>,
  warned_fallback: bool,
}

impl<P: AudioStreamFactory> ResilientStreamManager<P> {
  pub fn new(provider: P, policy: RestartPolicy, now: Instant) -> Self {
    Self {
      policy,
      provider,
      state: RestartState::Restarting {
        failures: 0,
        next_attempt_at: now,
      },
      stream: None,
      warned_fallback: false,
    }
  }

  pub fn new_running(provider: P, policy: RestartPolicy, stream: P::Stream) -> Self {
    Self {
      policy,
      provider,
      state: RestartState::Running,
      stream: Some(stream),
      warned_fallback: false,
    }
  }

  pub fn state(&self) -> RestartState {
    self.state
  }

  pub fn next_attempt_at(&self) -> Option<Instant> {
    match self.state {
      RestartState::Restarting {
        next_attempt_at, ..
      } => Some(next_attempt_at),
      _ => None,
    }
  }

  /// Drop the current stream and schedule an immediate restart attempt.
  pub fn request_restart(&mut self, now: Instant) {
    if matches!(self.state, RestartState::Fallback) {
      return;
    }
    self.stream = None;
    self.state = RestartState::Restarting {
      failures: 0,
      next_attempt_at: now,
    };
  }

  /// Drive the state machine forward.
  pub fn tick(&mut self, now: Instant) -> TickOutcome {
    let mut outcome = TickOutcome::default();

    match self.state {
      RestartState::Running => {}
      RestartState::Fallback => {}
      RestartState::Restarting {
        mut failures,
        next_attempt_at,
      } => {
        if now < next_attempt_at {
          return outcome;
        }

        match self.provider.open_default_stream() {
          Ok(stream) => {
            self.stream = Some(stream);
            self.state = RestartState::Running;
            outcome.opened_stream = true;
          }
          Err(_err) => {
            failures = failures.saturating_add(1);
            if failures >= self.policy.max_attempts {
              self.stream = None;
              self.state = RestartState::Fallback;
              if !self.warned_fallback {
                outcome.entered_fallback = true;
                self.warned_fallback = true;
              }
              return outcome;
            }
            let backoff = self.policy.backoff_for_failure(failures);
            self.stream = None;
            self.state = RestartState::Restarting {
              failures,
              next_attempt_at: now + backoff,
            };
          }
        }
      }
    }

    outcome
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::VecDeque;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  struct FakeStream(u32);

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  struct FakeError(&'static str);

  struct FakeProvider {
    queue: VecDeque<Result<FakeStream, FakeError>>,
    calls: usize,
  }

  impl FakeProvider {
    fn new(queue: Vec<Result<FakeStream, FakeError>>) -> Self {
      Self {
        queue: queue.into(),
        calls: 0,
      }
    }
  }

  impl AudioStreamFactory for FakeProvider {
    type Stream = FakeStream;
    type Error = FakeError;

    fn open_default_stream(&mut self) -> Result<Self::Stream, Self::Error> {
      self.calls += 1;
      self.queue.pop_front().unwrap_or(Err(FakeError("no more")))
    }
  }

  #[test]
  fn restart_uses_exponential_backoff_and_recovers() {
    let t0 = Instant::now();
    let policy = RestartPolicy {
      max_attempts: 5,
      initial_backoff: Duration::from_millis(10),
      max_backoff: Duration::from_millis(100),
    };
    let provider = FakeProvider::new(vec![
      Err(FakeError("missing device")),
      Err(FakeError("still missing")),
      Ok(FakeStream(1)),
    ]);
    let mut manager = ResilientStreamManager::new(provider, policy, t0);

    // Immediate attempt at t0.
    let out = manager.tick(t0);
    assert_eq!(out.opened_stream, false);
    assert_eq!(out.entered_fallback, false);
    assert_eq!(manager.provider.calls, 1);
    assert!(matches!(
      manager.state(),
      RestartState::Restarting { failures: 1, .. }
    ));
    let next = manager.next_attempt_at().expect("scheduled retry");
    assert_eq!(next, t0 + Duration::from_millis(10));

    // Before the next attempt, no further open calls should be made.
    manager.tick(t0 + Duration::from_millis(5));
    assert_eq!(manager.provider.calls, 1);

    // Second failure at t0+10ms; backoff doubles to 20ms.
    manager.tick(t0 + Duration::from_millis(10));
    assert_eq!(manager.provider.calls, 2);
    let next = manager.next_attempt_at().expect("scheduled retry");
    assert_eq!(next, t0 + Duration::from_millis(30));

    // Recovery at t0+30ms.
    let out = manager.tick(t0 + Duration::from_millis(30));
    assert_eq!(manager.provider.calls, 3);
    assert_eq!(out.opened_stream, true);
    assert_eq!(manager.state(), RestartState::Running);
  }

  #[test]
  fn restart_enters_fallback_after_max_attempts_and_warns_once() {
    let t0 = Instant::now();
    let policy = RestartPolicy {
      max_attempts: 3,
      initial_backoff: Duration::from_millis(10),
      max_backoff: Duration::from_millis(100),
    };
    let provider = FakeProvider::new(vec![
      Err(FakeError("fail 1")),
      Err(FakeError("fail 2")),
      Err(FakeError("fail 3")),
      Ok(FakeStream(1)),
    ]);
    let mut manager = ResilientStreamManager::new(provider, policy, t0);

    // Failure #1 at t0.
    let out = manager.tick(t0);
    assert_eq!(out.entered_fallback, false);
    assert_eq!(
      manager.state(),
      RestartState::Restarting {
        failures: 1,
        next_attempt_at: t0 + Duration::from_millis(10),
      }
    );

    // Failure #2 at t0+10ms.
    let out = manager.tick(t0 + Duration::from_millis(10));
    assert_eq!(out.entered_fallback, false);
    assert_eq!(
      manager.state(),
      RestartState::Restarting {
        failures: 2,
        next_attempt_at: t0 + Duration::from_millis(30),
      }
    );

    // Failure #3 at t0+30ms => fallback.
    let out = manager.tick(t0 + Duration::from_millis(30));
    assert_eq!(out.entered_fallback, true);
    assert_eq!(manager.state(), RestartState::Fallback);
    assert_eq!(manager.provider.calls, 3);

    // Further ticks should do nothing (no new open calls, and no repeated "entered_fallback" events).
    let out = manager.tick(t0 + Duration::from_millis(10_000));
    assert_eq!(out.entered_fallback, false);
    assert_eq!(manager.provider.calls, 3);
  }

  #[test]
  fn request_restart_drops_stream_and_resets_failures() {
    let t0 = Instant::now();
    let policy = RestartPolicy {
      max_attempts: 5,
      initial_backoff: Duration::from_millis(1),
      max_backoff: Duration::from_millis(10),
    };
    let provider = FakeProvider::new(vec![
      Ok(FakeStream(1)),
      Err(FakeError("fail after error")),
      Ok(FakeStream(2)),
    ]);
    let mut manager = ResilientStreamManager::new(provider, policy, t0);

    // Initial open.
    let out = manager.tick(t0);
    assert_eq!(out.opened_stream, true);
    assert_eq!(manager.state(), RestartState::Running);
    assert!(manager.stream.is_some());

    // Simulate stream error: should drop stream and retry immediately (fail once).
    manager.request_restart(t0 + Duration::from_millis(5));
    assert!(manager.stream.is_none());
    assert!(matches!(
      manager.state(),
      RestartState::Restarting { failures: 0, .. }
    ));
    manager.tick(t0 + Duration::from_millis(5));
    assert!(matches!(
      manager.state(),
      RestartState::Restarting { failures: 1, .. }
    ));

    // Next attempt succeeds after backoff.
    let next = manager.next_attempt_at().expect("scheduled retry");
    let out = manager.tick(next);
    assert_eq!(out.opened_stream, true);
    assert_eq!(manager.state(), RestartState::Running);
  }
}
