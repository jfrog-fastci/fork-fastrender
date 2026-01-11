use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TimerKey(u64);

/// A simple timer scheduler intended to back higher-level runtimes.
///
/// Semantics:
/// - Timers never fire early: a timer scheduled for `deadline` only fires when polled with
///   `now >= deadline`.
/// - Cancellation is idempotent: canceling a stale/unknown key returns `None`.
/// - [`TimerWheel::next_deadline`] always reports the earliest scheduled deadline among active timers.
///
/// The implementation is deliberately straightforward (a `BTreeMap` + `HashMap`) so we can validate
/// semantics first. More sophisticated wheels can be substituted later as long as they preserve the
/// public API and behaviour.
pub struct TimerWheel<T> {
  next_key: u64,
  timers: HashMap<TimerKey, TimerData<T>>,
  by_deadline: BTreeMap<Instant, Vec<TimerKey>>,
}

struct TimerData<T> {
  deadline: Instant,
  index: usize,
  payload: T,
}

impl<T> Default for TimerWheel<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> TimerWheel<T> {
  pub fn new() -> Self {
    Self {
      next_key: 0,
      timers: HashMap::new(),
      by_deadline: BTreeMap::new(),
    }
  }

  pub fn schedule(&mut self, deadline: Instant, payload: T) -> TimerKey {
    let key = TimerKey(self.next_key);
    self.next_key += 1;

    let keys = self.by_deadline.entry(deadline).or_default();
    let index = keys.len();
    keys.push(key);

    self.timers.insert(
      key,
      TimerData {
        deadline,
        index,
        payload,
      },
    );
    key
  }

  pub fn cancel(&mut self, key: TimerKey) -> Option<T> {
    let TimerData {
      deadline,
      index,
      payload,
    } = self.timers.remove(&key)?;

    if let Some(keys) = self.by_deadline.get_mut(&deadline) {
      // Fast path: we remember the index within the per-deadline vector so cancellation does not
      // require a linear search.
      let removed_index = if index < keys.len() && keys[index] == key {
        keys.swap_remove(index);
        index
      } else if let Some(pos) = keys.iter().position(|k| *k == key) {
        // Should not happen, but fall back to a linear scan if internal indices drift.
        keys.swap_remove(pos);
        pos
      } else {
        // Also unexpected: the deadline bucket exists but does not contain this key.
        // Leave the bucket untouched.
        keys.len()
      };

      if removed_index < keys.len() {
        let moved_key = keys[removed_index];
        if let Some(data) = self.timers.get_mut(&moved_key) {
          data.index = removed_index;
        }
      }
      if keys.is_empty() {
        self.by_deadline.remove(&deadline);
      }
    }

    Some(payload)
  }

  pub fn poll_expired<F: FnMut(T)>(&mut self, now: Instant, mut on_fired: F) {
    while let Some((&deadline, _)) = self.by_deadline.iter().next() {
      if deadline > now {
        break;
      }

      let keys = self.by_deadline.remove(&deadline).unwrap();
      for key in keys {
        if let Some(data) = self.timers.remove(&key) {
          on_fired(data.payload);
        }
      }
    }
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.by_deadline.keys().next().copied()
  }
}
