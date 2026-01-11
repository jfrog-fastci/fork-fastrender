use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use crate::timer_wheel::TimerKey;
use crate::timer_wheel::TimerWheel;

/// Timer driver used by [`crate::ReactorDriver`].
///
/// This is a thin wrapper around the underlying timer wheel. It provides the
/// primitives needed by the reactor to determine the next sleep duration and to
/// collect expired timers.
pub struct TimerDriver {
  wheel: TimerWheel<Waker>,
  len: usize,
}

impl TimerDriver {
  pub fn new() -> Self {
    Self::new_at(Instant::now())
  }

  pub fn new_at(base: Instant) -> Self {
    Self {
      wheel: TimerWheel::new_at(base),
      len: 0,
    }
  }

  pub fn insert(&mut self, deadline: Instant, waker: Waker) -> TimerKey {
    self.len += 1;
    self.wheel.schedule(deadline, waker)
  }

  pub fn cancel(&mut self, key: TimerKey) -> bool {
    if self.wheel.cancel(key).is_some() {
      self.len -= 1;
      true
    } else {
      false
    }
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.wheel.next_deadline()
  }

  pub fn time_until_next_deadline(&self, now: Instant) -> Option<Duration> {
    let deadline = self.next_deadline()?;
    Some(deadline.saturating_duration_since(now))
  }

  pub fn poll_expired(&mut self, now: Instant) -> Vec<Waker> {
    let mut out = Vec::new();
    self.wheel.poll_expired(now, |waker| {
      out.push(waker);
      self.len -= 1;
    });
    out
  }

  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  pub fn len(&self) -> usize {
    self.len
  }
}
