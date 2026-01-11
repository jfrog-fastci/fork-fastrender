use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use std::time::Instant;

use proptest::prelude::*;

use runtime_native::{TimerKey, TimerWheel};
use runtime_native::test_util::TestRuntimeGuard;

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
    self
      .by_deadline
      .entry(deadline)
      .or_default()
      .push((key, id));
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
    let _rt = TestRuntimeGuard::new();
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

#[test]
fn regression_de09a7a0_missing_timer_fire() {
  // Regression for a historical proptest failure (see
  // `timer_wheel_model.proptest-regressions`).
  let _rt = TestRuntimeGuard::new();
  let ops = vec![
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 0 },
    Op::Schedule { after_ms: 15 },
    Op::Cancel { key_index: 6520 },
    Op::Advance { delta_ms: 150_192 },
    Op::Schedule { after_ms: 1236 },
    Op::Schedule { after_ms: 1753 },
    Op::Schedule { after_ms: 1007 },
    Op::Advance { delta_ms: 590_276 },
    Op::Schedule { after_ms: 49 },
    Op::Schedule { after_ms: 4328 },
    Op::Cancel { key_index: 730 },
    Op::Schedule { after_ms: 8484 },
    Op::Schedule { after_ms: 5139 },
    Op::Schedule { after_ms: 2139 },
    Op::Schedule { after_ms: 5919 },
    Op::Schedule { after_ms: 563 },
    Op::Schedule { after_ms: 4538 },
    Op::Cancel { key_index: 8555 },
    Op::Schedule { after_ms: 9233 },
    Op::Advance { delta_ms: 25_964 },
    Op::Cancel { key_index: 5289 },
    Op::Schedule { after_ms: 9320 },
    Op::Schedule { after_ms: 9803 },
    Op::Advance { delta_ms: 212_389 },
    Op::Schedule { after_ms: 4634 },
    Op::Schedule { after_ms: 6897 },
    Op::Schedule { after_ms: 8508 },
    Op::Schedule { after_ms: 7059 },
    Op::Advance { delta_ms: 340_301 },
    Op::Cancel { key_index: 4613 },
    Op::Schedule { after_ms: 9813 },
    Op::Cancel { key_index: 8851 },
    Op::Advance { delta_ms: 982_802 },
    Op::Cancel { key_index: 6383 },
    Op::Advance { delta_ms: 317_567 },
    Op::Advance { delta_ms: 492_640 },
    Op::Schedule { after_ms: 6245 },
    Op::Schedule { after_ms: 7972 },
    Op::Advance { delta_ms: 184_680 },
    Op::Advance { delta_ms: 501_627 },
    Op::Advance { delta_ms: 346_226 },
    Op::Schedule { after_ms: 2201 },
    Op::Schedule { after_ms: 703 },
    Op::Schedule { after_ms: 9793 },
    Op::Cancel { key_index: 546 },
    Op::Schedule { after_ms: 2217 },
    Op::Advance { delta_ms: 545_159 },
    Op::Cancel { key_index: 253 },
    Op::Schedule { after_ms: 9977 },
    Op::Schedule { after_ms: 9937 },
    Op::Schedule { after_ms: 6869 },
    Op::Schedule { after_ms: 2801 },
    Op::Schedule { after_ms: 7200 },
    Op::Cancel { key_index: 2946 },
    Op::Schedule { after_ms: 5664 },
    Op::Advance { delta_ms: 707_145 },
    Op::Advance { delta_ms: 128_003 },
    Op::Advance { delta_ms: 604_248 },
    Op::Advance { delta_ms: 629_520 },
    Op::Advance { delta_ms: 444_311 },
    Op::Schedule { after_ms: 4818 },
    Op::Schedule { after_ms: 5304 },
    Op::Advance { delta_ms: 706_645 },
    Op::Schedule { after_ms: 9833 },
    Op::Advance { delta_ms: 80_098 },
    Op::Schedule { after_ms: 6100 },
    Op::Advance { delta_ms: 406_612 },
    Op::Schedule { after_ms: 4084 },
    Op::Advance { delta_ms: 595_923 },
    Op::Schedule { after_ms: 425 },
    Op::Cancel { key_index: 6923 },
    Op::Schedule { after_ms: 6762 },
    Op::Schedule { after_ms: 6801 },
    Op::Schedule { after_ms: 8322 },
    Op::Advance { delta_ms: 791_551 },
    Op::Advance { delta_ms: 34_320 },
    Op::Schedule { after_ms: 2051 },
    Op::Schedule { after_ms: 8338 },
    Op::Cancel { key_index: 892 },
    Op::Advance { delta_ms: 161_063 },
    Op::Schedule { after_ms: 7297 },
    Op::Advance { delta_ms: 651_253 },
    Op::Schedule { after_ms: 5322 },
    Op::Advance { delta_ms: 180_070 },
    Op::Cancel { key_index: 9059 },
    Op::Advance { delta_ms: 924_705 },
    Op::Advance { delta_ms: 600_825 },
    Op::Schedule { after_ms: 9487 },
    Op::Advance { delta_ms: 418_805 },
  ];

  let base = Instant::now();
  let mut now_ms: u64 = 0;

  let mut wheel = TimerWheel::<u64>::new();
  let mut reference = RefScheduler::new();

  let mut keys = Vec::<TimerKey>::new();
  let mut next_id: u64 = 0;

  for (step, op) in ops.into_iter().enumerate() {
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
        if !keys.is_empty() {
          let key = keys[(key_index as usize) % keys.len()];
          let wheel_res = wheel.cancel(key);
          let ref_res = reference.cancel(key);
          assert_eq!(wheel_res, ref_res, "cancel mismatch at step {step}");
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
        assert_eq!(
          wheel_fired, ref_fired,
          "poll mismatch at step {step}: now_ms={now_ms}"
        );
      }
    }

    assert_eq!(
      wheel.next_deadline(),
      reference.next_deadline(),
      "next_deadline mismatch at step {step}: now_ms={now_ms}"
    );
  }
}
