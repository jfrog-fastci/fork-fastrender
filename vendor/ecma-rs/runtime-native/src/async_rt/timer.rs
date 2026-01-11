use crate::async_rt::Task;
use std::collections::HashMap;
use crate::timer_wheel::{TimerKey, TimerWheel};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

impl TimerId {
  pub fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  pub fn as_raw(self) -> u64 {
    self.0
  }
}

pub struct Timers {
  next_id: AtomicU64,
  inner: Mutex<TimersInner>,
}

struct TimersInner {
  wheel: TimerWheel<TimerEntry>,
  keys: HashMap<TimerId, TimerKey>,
}

struct TimerEntry {
  id: TimerId,
  task: Task,
}

impl Timers {
  pub fn new() -> Self {
    Self {
      next_id: AtomicU64::new(1),
      inner: Mutex::new(TimersInner {
        wheel: TimerWheel::new(),
        keys: HashMap::new(),
      }),
    }
  }

  pub fn has_timers(&self) -> bool {
    !self.inner.lock().unwrap().keys.is_empty()
  }

  pub fn len(&self) -> usize {
    self.inner.lock().unwrap().keys.len()
  }

  pub fn schedule(&self, deadline: Instant, task: Task) -> TimerId {
    let id = TimerId(self.next_id.fetch_add(1, AtomicOrdering::Relaxed));
    let mut inner = self.inner.lock().unwrap();
    let key = inner.wheel.schedule(deadline, TimerEntry { id, task });
    let prev = inner.keys.insert(id, key);
    debug_assert!(prev.is_none(), "timer id collision");
    crate::rt_trace::timer_heap_inc_by(1);
    id
  }

  pub fn cancel(&self, id: TimerId) -> bool {
    let mut inner = self.inner.lock().unwrap();
    let Some(key) = inner.keys.remove(&id) else {
      return false;
    };
    let prev = inner.wheel.cancel(key);
    debug_assert!(prev.is_some(), "timer key missing from wheel");
    crate::rt_trace::timer_heap_dec_by(1);
    true
  }

  pub fn drain_due(&self, now: Instant) -> Vec<Task> {
    let mut ready = Vec::new();
    let mut inner = self.inner.lock().unwrap();
    let mut fired_ids = Vec::new();
    inner.wheel.poll_expired(now, |entry| {
      fired_ids.push(entry.id);
      ready.push(entry.task);
    });
    for id in fired_ids {
      let removed = inner.keys.remove(&id);
      debug_assert!(removed.is_some(), "timer id missing from key map");
    }
    if !ready.is_empty() {
      crate::rt_trace::timer_heap_dec_by(ready.len() as u64);
    }
    ready
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    self.inner.lock().unwrap().wheel.next_deadline()
  }

  pub fn clear(&self) {
    let mut inner = self.inner.lock().unwrap();
    let active = inner.keys.len() as u64;
    inner.wheel = TimerWheel::new();
    inner.keys.clear();
    if active > 0 {
      crate::rt_trace::timer_heap_dec_by(active);
    }
  }
}

impl Drop for Timers {
  fn drop(&mut self) {
    let active = self.inner.lock().unwrap().keys.len() as u64;
    if active > 0 {
      crate::rt_trace::timer_heap_dec_by(active);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rand::rngs::StdRng;
  use rand::{Rng, SeedableRng};
  use std::collections::HashMap;
  use std::time::Duration;

  struct RecordCtx {
    payload: u64,
    out: *const Mutex<Vec<u64>>,
  }

  extern "C" fn record_fire(data: *mut u8) {
    // Safety: `data` is a stable pointer to a `RecordCtx` owned by the `Task`.
    let ctx = unsafe { &*(data as *const RecordCtx) };
    let out = unsafe { &*ctx.out };
    out.lock().unwrap().push(ctx.payload);
  }

  extern "C" fn record_drop(data: *mut u8) {
    // Safety: `data` was allocated by `Box::into_raw` in `record_task`.
    unsafe {
      drop(Box::from_raw(data as *mut RecordCtx));
    }
  }

  fn record_task(payload: u64, out: *const Mutex<Vec<u64>>) -> Task {
    let ctx = Box::new(RecordCtx { payload, out });
    Task::new_with_drop(record_fire, Box::into_raw(ctx).cast(), record_drop)
  }

  #[test]
  fn timers_matches_reference_model() {
    let timers = Timers::new();
    let fired: &'static Mutex<Vec<u64>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let fired_ptr = fired as *const _;

    // Keep all instants derived from a single base so we can reason about ordering.
    let base = Instant::now();
    let mut now_ms: u64 = 0;

    let mut rng = StdRng::seed_from_u64(0x494_494_494);

    // Reference scheduler: TimerId -> (deadline, payload).
    let mut reference: HashMap<TimerId, (Instant, u64)> = HashMap::new();
    let mut ids: Vec<TimerId> = Vec::new();
    let mut next_payload: u64 = 0;

    for _step in 0..1000 {
      let op = rng.random_range(0u8..100);

      if op < 55 {
        // Schedule
        let after_ms = rng.random_range(0u64..=10_000);
        let deadline = base + Duration::from_millis(now_ms + after_ms);
        let payload = next_payload;
        next_payload += 1;

        let id = timers.schedule(deadline, record_task(payload, fired_ptr));
        reference.insert(id, (deadline, payload));
        ids.push(id);
      } else if op < 80 {
        // Cancel
        if !ids.is_empty() {
          let id = ids[rng.random_range(0..ids.len())];
          let existed = timers.cancel(id);
          let expected = reference.remove(&id).is_some();
          assert_eq!(existed, expected);
        }
      } else {
        // Advance time + drain due timers.
        let delta_ms = rng.random_range(0u64..=50_000);
        now_ms += delta_ms;
        let now = base + Duration::from_millis(now_ms);

        let mut expected_payloads = Vec::new();
        let mut to_remove = Vec::new();
        for (&id, &(deadline, payload)) in reference.iter() {
          if deadline <= now {
            to_remove.push(id);
            expected_payloads.push(payload);
          }
        }
        for id in &to_remove {
          reference.remove(id);
        }
        expected_payloads.sort_unstable();

        fired.lock().unwrap().clear();
        let due = timers.drain_due(now);
        for task in due {
          task.run();
        }
        let mut got = std::mem::take(&mut *fired.lock().unwrap());
        got.sort_unstable();
        assert_eq!(got, expected_payloads);
      }

      // Invariants: `len`, `has_timers`, and `next_deadline`.
      assert_eq!(timers.len(), reference.len());
      assert_eq!(timers.has_timers(), !reference.is_empty());

      let expected_deadline = reference
        .values()
        .map(|(deadline, _)| *deadline)
        .min();
      assert_eq!(timers.next_deadline(), expected_deadline);
    }

    // Clearing should drop any remaining tasks and reset next_deadline.
    timers.clear();
    assert_eq!(timers.len(), 0);
    assert!(!timers.has_timers());
    assert_eq!(timers.next_deadline(), None);

    // Ensure the timer tasks were dropped (no pending record contexts).
    // We can't directly observe drops here, but `clear` should make cancellation idempotent.
    for id in ids {
      assert!(!timers.cancel(id));
    }
  }
}
