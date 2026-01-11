use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use runtime_native::{TimerKey, TimerWheel};

#[derive(Clone, Debug)]
enum Op {
  Schedule { after_ms: u32 },
  Cancel { key_index: u32 },
  Advance { delta_ms: u64 },
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
  prop::collection::vec(
    prop_oneof![
      5 => (0u32..=10_000).prop_map(|after_ms| Op::Schedule { after_ms }),
      3 => (0u32..=10_000).prop_map(|key_index| Op::Cancel { key_index }),
      4 => (0u64..=1_000_000).prop_map(|delta_ms| Op::Advance { delta_ms }),
    ],
    100..300,
  )
}

struct RefScheduler {
  active: HashMap<TimerKey, (Instant, u64)>,
  by_deadline: BTreeMap<Instant, Vec<(TimerKey, u64)>>,
}

impl RefScheduler {
  fn new() -> Self {
    Self {
      active: HashMap::new(),
      by_deadline: BTreeMap::new(),
    }
  }

  fn schedule(&mut self, key: TimerKey, deadline: Instant, id: u64) {
    self.active.insert(key, (deadline, id));
    self.by_deadline.entry(deadline).or_default().push((key, id));
  }

  fn cancel(&mut self, key: TimerKey) -> Option<u64> {
    let (deadline, id) = self.active.remove(&key)?;

    if let Some(entries) = self.by_deadline.get_mut(&deadline) {
      if let Some(pos) = entries.iter().position(|(k, _)| *k == key) {
        entries.swap_remove(pos);
      }
      if entries.is_empty() {
        self.by_deadline.remove(&deadline);
      }
    }

    Some(id)
  }

  fn poll_expired(&mut self, now: Instant) -> Vec<u64> {
    let deadlines: Vec<Instant> = self.by_deadline.range(..=now).map(|(&d, _)| d).collect();

    let mut fired = Vec::new();
    for deadline in deadlines {
      let entries = self.by_deadline.remove(&deadline).unwrap();
      for (key, id) in entries {
        if self.active.remove(&key).is_some() {
          fired.push(id);
        }
      }
    }

    fired
  }

  fn next_deadline(&self) -> Option<Instant> {
    self.by_deadline.keys().next().copied()
  }
}

proptest! {
  #![proptest_config(ProptestConfig {
    cases: 128,
    .. ProptestConfig::default()
  })]

  #[test]
  fn timer_wheel_matches_reference(ops in ops_strategy()) {
    // A random `Instant::now()` base is fine (and unavoidable); we keep all times as
    // `base + N * 1ms` so the test doesn't depend on any internal tick rounding.
    let base = Instant::now();
    let mut now_ms: u64 = 0;

    let mut wheel = TimerWheel::<u64>::new();
    let mut reference = RefScheduler::new();

    let mut keys = Vec::<TimerKey>::new();
    let mut next_id: u64 = 0;

    for op in ops {
      match op {
        Op::Schedule { after_ms } => {
          let deadline_ms = now_ms + after_ms as u64;
          let deadline = base + Duration::from_millis(deadline_ms);

          let id = next_id;
          next_id += 1;

          let key = wheel.schedule(deadline, id);
          reference.schedule(key, deadline, id);
          keys.push(key);
        }

        Op::Cancel { key_index } => {
          if keys.is_empty() {
            // No keys have been created yet; treat as a no-op.
          } else {
            let key = keys[(key_index as usize) % keys.len()];
            let wheel_res = wheel.cancel(key);
            let ref_res = reference.cancel(key);
            prop_assert_eq!(wheel_res, ref_res);
          }
        }

        Op::Advance { delta_ms } => {
          now_ms += delta_ms;
          let now = base + Duration::from_millis(now_ms);

          let mut wheel_fired = Vec::new();
          wheel.poll_expired(now, |id| wheel_fired.push(id));

          let mut ref_fired = reference.poll_expired(now);

          wheel_fired.sort_unstable();
          ref_fired.sort_unstable();
          prop_assert_eq!(wheel_fired, ref_fired);
        }
      }

      prop_assert_eq!(wheel.next_deadline(), reference.next_deadline());
    }
  }
}

