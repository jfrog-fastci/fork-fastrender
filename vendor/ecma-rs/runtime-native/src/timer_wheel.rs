use std::task::Waker;
use std::time::Instant;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TimerKey(u64);

/// Helper payload for scheduling wakers into the timer wheel.
#[derive(Debug, Clone)]
pub struct WakeTask(pub Waker);

impl WakeTask {
  #[inline]
  pub fn wake(self) {
    self.0.wake();
  }
}

// 64^6 ~= 6.87e10ms ~= 795 days, enough to cover JS' max timeout (2^31-1 ms).
const BITS: usize = 6;
const SLOTS: usize = 1 << BITS; // 64
const LEVELS: usize = 6;
const MASK: u64 = (SLOTS as u64) - 1;

impl TimerKey {
  #[inline]
  fn from_parts(index: u32, generation: u32) -> Self {
    Self(((generation as u64) << 32) | (index as u64))
  }

  #[inline]
  fn index(self) -> u32 {
    (self.0 & 0xFFFF_FFFF) as u32
  }

  #[inline]
  fn generation(self) -> u32 {
    (self.0 >> 32) as u32
  }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DebugStats {
  /// How many times `poll_expired` advanced the internal tick counter.
  pub tick_steps: u64,
  /// How many wheel buckets were processed (expired or cascaded).
  pub slot_visits: u64,
  /// How many cascades were performed (a higher-level bucket drained and reinserted).
  pub cascades: u64,
}

#[derive(Clone, Copy)]
struct Bucket {
  head: Option<u32>,
  tail: Option<u32>,
  min_deadline: Option<Instant>,
}

impl Default for Bucket {
  fn default() -> Self {
    Self {
      head: None,
      tail: None,
      min_deadline: None,
    }
  }
}

struct Level {
  buckets: [Bucket; SLOTS],
  occupied: u64, // bit i set iff buckets[i] is non-empty
}

impl Default for Level {
  fn default() -> Self {
    Self {
      buckets: std::array::from_fn(|_| Bucket::default()),
      occupied: 0,
    }
  }
}

struct Entry<T> {
  deadline: Instant,
  when: u64,
  payload: T,

  prev: Option<u32>,
  next: Option<u32>,
  level: u8,
  slot: u8,
}

struct Slot<T> {
  gen: u32,
  entry: Option<Entry<T>>,
  next_free: Option<u32>,
}

/// A timer scheduler intended to back higher-level runtimes.
///
/// Semantics:
/// - Timers never fire early: a timer scheduled for `deadline` only fires when polled with
///   `now >= deadline`.
/// - Cancellation is idempotent: canceling a stale/unknown key returns `None`.
/// - [`TimerWheel::next_deadline`] always reports the earliest scheduled deadline among active timers.
///
/// Implementation: a tokio-style hierarchical hashed timer wheel:
/// - 6 levels × 64 slots (1ms base tick).
/// - per-slot intrusive lists backed by an arena with generational indices in [`TimerKey`].
pub struct TimerWheel<T> {
  base: Instant,
  current_tick: u64,
  levels: [Level; LEVELS],
  slots: Vec<Slot<T>>,
  free_head: Option<u32>,

  #[cfg(test)]
  stats: DebugStats,
}

impl<T> Default for TimerWheel<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> TimerWheel<T> {
  pub fn new() -> Self {
    Self::new_at(Instant::now())
  }

  /// Construct a timer wheel using `base` as the tick origin.
  ///
  /// This is useful for deterministic tests: callers can derive all deadlines from a single base
  /// instant and advance time without sleeping.
  pub fn new_at(base: Instant) -> Self {
    Self {
      base,
      current_tick: 0,
      levels: std::array::from_fn(|_| Level::default()),
      slots: Vec::new(),
      free_head: None,
      #[cfg(test)]
      stats: DebugStats::default(),
    }
  }

  pub fn schedule(&mut self, deadline: Instant, payload: T) -> TimerKey {
    let when = self.tick_for(deadline).max(self.current_tick);
    let entry = Entry {
      deadline,
      when,
      payload,
      prev: None,
      next: None,
      level: 0,
      slot: 0,
    };

    let (idx, gen) = self.alloc(entry);
    self.insert_idx(idx);
    #[cfg(debug_assertions)]
    self.debug_check();
    TimerKey::from_parts(idx, gen)
  }

  pub fn cancel(&mut self, key: TimerKey) -> Option<T> {
    let idx = key.index();
    let slot_idx = idx as usize;
    let (level, bucket) = {
      let slot = self.slots.get(slot_idx)?;
      if slot.gen != key.generation() {
        return None;
      }
      let entry = slot.entry.as_ref()?;
      (entry.level as usize, entry.slot as usize)
    };

    self.unlink_idx(level, bucket, idx);
    let entry = self.slots[slot_idx].entry.take().unwrap();
    self.free(idx);
    #[cfg(debug_assertions)]
    self.debug_check();
    Some(entry.payload)
  }

  pub fn poll_expired<F: FnMut(T)>(&mut self, now: Instant, mut on_fired: F) {
    #[cfg(test)]
    {
      self.stats = DebugStats::default();
    }

    // Only advance forward; callers should pass monotonic `now`, but be defensive.
    let target_tick = self.tick_for(now).max(self.current_tick);

    // Always process the current slot. If we're still in the same tick (e.g. woke early),
    // this allows timers with deadlines inside the tick to be fired once `now` reaches them.
    self.process_level0_slot(now, target_tick, &mut on_fired);

    while self.current_tick < target_tick {
      let Some(next_tick) = self.next_event_tick(target_tick) else {
        // Nothing to do before `target_tick`; jump directly.
        self.advance_tick(target_tick);
        break;
      };

      self.advance_tick(next_tick);
      self.cascade_at_current_tick();
      self.process_level0_slot(now, target_tick, &mut on_fired);
    }

    // We may have jumped to `target_tick` without processing its bucket; process again.
    self.process_level0_slot(now, target_tick, &mut on_fired);

    #[cfg(debug_assertions)]
    self.debug_check();
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    let mut min: Option<Instant> = None;
    for lvl in &self.levels {
      for bucket in &lvl.buckets {
        if let Some(d) = bucket.min_deadline {
          min = Some(match min {
            Some(cur) if cur <= d => cur,
            _ => d,
          });
        }
      }
    }
    min
  }

  #[inline]
  fn tick_for(&self, t: Instant) -> u64 {
    let Some(dur) = t.checked_duration_since(self.base) else {
      return 0;
    };
    let ms = dur.as_millis();
    if ms > u64::MAX as u128 {
      return u64::MAX;
    }
    ms as u64
  }

  #[inline]
  fn advance_tick(&mut self, new_tick: u64) {
    if new_tick == self.current_tick {
      return;
    }
    debug_assert!(new_tick > self.current_tick);
    self.current_tick = new_tick;
    #[cfg(test)]
    {
      self.stats.tick_steps += 1;
    }
  }

  fn alloc(&mut self, entry: Entry<T>) -> (u32, u32) {
    if let Some(idx) = self.free_head {
      let slot = &mut self.slots[idx as usize];
      self.free_head = slot.next_free;
      slot.next_free = None;
      debug_assert!(slot.entry.is_none());
      slot.entry = Some(entry);
      return (idx, slot.gen);
    }

    let idx = self.slots.len() as u32;
    self.slots.push(Slot {
      gen: 0,
      entry: Some(entry),
      next_free: None,
    });
    (idx, 0)
  }

  fn free(&mut self, idx: u32) {
    let slot = &mut self.slots[idx as usize];
    debug_assert!(slot.entry.is_none());
    slot.gen = slot.gen.wrapping_add(1);
    slot.next_free = self.free_head;
    self.free_head = Some(idx);
  }

  #[inline]
  fn level_for(&self, when: u64) -> usize {
    let diff = when.saturating_sub(self.current_tick);
    let mut level = 0usize;
    let mut max = 1u64 << BITS;
    while level < LEVELS - 1 && diff >= max {
      level += 1;
      max <<= BITS;
    }
    level
  }

  #[inline]
  fn bucket_for(level: usize, when: u64) -> usize {
    ((when >> (level * BITS)) & MASK) as usize
  }

  fn insert_idx(&mut self, idx: u32) {
    let when;
    let deadline;
    {
      let entry = self.slots[idx as usize].entry.as_ref().unwrap();
      when = entry.when;
      deadline = entry.deadline;
    }

    let level = self.level_for(when);
    let bucket = Self::bucket_for(level, when);

    {
      let entry = self.slots[idx as usize].entry.as_mut().unwrap();
      entry.level = level as u8;
      entry.slot = bucket as u8;
      entry.prev = None;
      entry.next = None;
    }

    self.link_idx(level, bucket, idx, deadline);
  }

  fn link_idx(&mut self, level: usize, bucket: usize, idx: u32, deadline: Instant) {
    let lvl = &mut self.levels[level];
    let b = &mut lvl.buckets[bucket];

    let was_empty = b.head.is_none();
    if was_empty {
      lvl.occupied |= 1u64 << bucket;
      b.head = Some(idx);
      b.tail = Some(idx);
    } else {
      // Push front.
      let old_head = b.head.unwrap();
      b.head = Some(idx);
      {
        let e = self.slots[idx as usize].entry.as_mut().unwrap();
        e.next = Some(old_head);
      }
      self.slots[old_head as usize].entry.as_mut().unwrap().prev = Some(idx);
    }

    b.min_deadline = Some(match b.min_deadline {
      Some(cur) if cur <= deadline => cur,
      _ => deadline,
    });
  }

  fn unlink_idx(&mut self, level: usize, bucket: usize, idx: u32) {
    let (prev, next, deadline) = {
      let e = self.slots[idx as usize].entry.as_ref().unwrap();
      (e.prev, e.next, e.deadline)
    };

    // Update neighbours / bucket head+tail.
    let lvl = &mut self.levels[level];
    let b = &mut lvl.buckets[bucket];

    match prev {
      Some(p) => {
        self.slots[p as usize].entry.as_mut().unwrap().next = next;
      }
      None => {
        b.head = next;
      }
    }

    match next {
      Some(n) => {
        self.slots[n as usize].entry.as_mut().unwrap().prev = prev;
      }
      None => {
        b.tail = prev;
      }
    }

    {
      let e = self.slots[idx as usize].entry.as_mut().unwrap();
      e.prev = None;
      e.next = None;
    }

    if b.head.is_none() {
      b.tail = None;
      b.min_deadline = None;
      lvl.occupied &= !(1u64 << bucket);
      return;
    }

    if b.min_deadline == Some(deadline) {
      self.recompute_bucket_min(level, bucket);
    }
  }

  fn recompute_bucket_min(&mut self, level: usize, bucket: usize) {
    let mut min: Option<Instant> = None;
    let mut node = self.levels[level].buckets[bucket].head;
    while let Some(idx) = node {
      let e = self.slots[idx as usize].entry.as_ref().unwrap();
      min = Some(match min {
        Some(cur) if cur <= e.deadline => cur,
        _ => e.deadline,
      });
      node = e.next;
    }
    self.levels[level].buckets[bucket].min_deadline = min;
  }

  fn drain_bucket(&mut self, level: usize, bucket: usize) -> Option<u32> {
    let lvl = &mut self.levels[level];
    let b = &mut lvl.buckets[bucket];
    let head = b.head?;
    b.head = None;
    b.tail = None;
    b.min_deadline = None;
    lvl.occupied &= !(1u64 << bucket);
    Some(head)
  }

  fn cascade_at_current_tick(&mut self) {
    let tick = self.current_tick;
    // Cascade from high to low so timers can move multiple levels in one boundary tick.
    for level in (1..LEVELS).rev() {
      let shift = level * BITS;
      let mask = (1u64 << shift) - 1;
      if tick != 0 && (tick & mask) == 0 {
        let bucket = ((tick >> shift) & MASK) as usize;
        self.cascade_bucket(level, bucket);
      }
    }
  }

  fn cascade_bucket(&mut self, level: usize, bucket: usize) {
    let Some(head) = self.drain_bucket(level, bucket) else {
      return;
    };

    #[cfg(test)]
    {
      self.stats.cascades += 1;
      self.stats.slot_visits += 1;
    }

    let mut node = Some(head);
    while let Some(idx) = node {
      let next = self.slots[idx as usize].entry.as_ref().unwrap().next;
      {
        let e = self.slots[idx as usize].entry.as_mut().unwrap();
        e.prev = None;
        e.next = None;
      }
      node = next;
      self.insert_idx(idx);
    }
  }

  #[inline]
  fn next_occupied_after(occupied: u64, start: usize) -> Option<usize> {
    debug_assert!(start < 64);
    let cleared = occupied & !(1u64 << start);
    if cleared == 0 {
      return None;
    }
    // Rotate so `start + 1` becomes bit 0, find the next set bit, then rotate back.
    let rot = cleared.rotate_right(((start + 1) & 63) as u32);
    let tz = rot.trailing_zeros() as usize;
    Some((tz + start + 1) & 63)
  }

  fn next_event_tick(&self, target_tick: u64) -> Option<u64> {
    let mut best: Option<u64> = None;

    for level in 0..LEVELS {
      let occ = self.levels[level].occupied;
      if occ == 0 {
        continue;
      }

      let shift = level * BITS;
      let slot_num = self.current_tick >> shift;
      let cur_idx = (slot_num & MASK) as usize;
      let mut cand: Option<u64> = None;

      if let Some(next_idx) = Self::next_occupied_after(occ, cur_idx) {
        let dist = if next_idx > cur_idx {
          next_idx - cur_idx
        } else {
          next_idx + SLOTS - cur_idx
        } as u64;
        cand = Some((slot_num + dist) << shift);
      } else if level != 0 && (occ & (1u64 << cur_idx)) != 0 {
        // The current bucket can legitimately contain timers for the *next* wheel cycle.
        // Example: at level 1 (64ms slots, 4096ms cycle), if `current_tick` is near the end
        // of the cycle and a timer is scheduled early in the next cycle, both map to the
        // same bucket index. In that case the bucket should be processed when the wheel
        // wraps (i.e. 64 slots ahead), not skipped forever.
        cand = Some((slot_num + (SLOTS as u64)) << shift);
      }

      let Some(cand) = cand else {
        continue;
      };
      if cand <= self.current_tick || cand > target_tick {
        continue;
      }
      best = Some(match best {
        Some(cur) if cur <= cand => cur,
        _ => cand,
      });
    }

    best
  }

  fn process_level0_slot<F: FnMut(T)>(&mut self, now: Instant, target_tick: u64, on_fired: &mut F) {
    #[cfg(test)]
    {
      self.stats.slot_visits += 1;
    }

    let bucket = (self.current_tick & MASK) as usize;
    let is_final_tick = self.current_tick == target_tick;

    // Fast-path: if not the final tick, everything in this bucket must be expired (since `now`
    // is in a strictly later tick), so drain it entirely.
    if !is_final_tick {
      let mut node = self.drain_bucket(0, bucket);
      while let Some(idx) = node {
        let next = self.slots[idx as usize].entry.as_ref().unwrap().next;
        let deadline = self.slots[idx as usize].entry.as_ref().unwrap().deadline;
        node = next;

        if deadline <= now {
          let payload = self.slots[idx as usize].entry.take().unwrap().payload;
          self.free(idx);
          on_fired(payload);
        } else {
          // Defensive: shouldn't happen when `current_tick < target_tick`, but keep semantics.
          {
            let e = self.slots[idx as usize].entry.as_mut().unwrap();
            e.prev = None;
            e.next = None;
          }
          self.insert_idx(idx);
        }
      }
      return;
    }

    // Final tick: only fire those with deadline <= now; keep the rest in the wheel.
    let mut node = self.drain_bucket(0, bucket);
    while let Some(idx) = node {
      let next = self.slots[idx as usize].entry.as_ref().unwrap().next;
      let deadline = self.slots[idx as usize].entry.as_ref().unwrap().deadline;
      node = next;

      if deadline <= now {
        let payload = self.slots[idx as usize].entry.take().unwrap().payload;
        self.free(idx);
        on_fired(payload);
      } else {
        {
          let e = self.slots[idx as usize].entry.as_mut().unwrap();
          e.prev = None;
          e.next = None;
        }
        self.insert_idx(idx);
      }
    }
  }

  #[cfg(debug_assertions)]
  fn debug_check(&self) {
    let mut seen = vec![false; self.slots.len()];

    for level in 0..LEVELS {
      let lvl = &self.levels[level];
      for bucket in 0..SLOTS {
        let b = &lvl.buckets[bucket];
        let bit_set = ((lvl.occupied >> bucket) & 1) != 0;
        assert_eq!(
          b.head.is_some(),
          bit_set,
          "occupied bit mismatch: level={level} bucket={bucket}"
        );

        match (b.head, b.tail) {
          (None, None) => {
            assert!(
              b.min_deadline.is_none(),
              "non-empty min_deadline in empty bucket: level={level} bucket={bucket}"
            );
            continue;
          }
          (Some(_), Some(_)) => {}
          _ => panic!("head/tail mismatch: level={level} bucket={bucket}"),
        }

        let mut node = b.head;
        let mut prev: Option<u32> = None;
        let mut computed_min: Option<Instant> = None;
        while let Some(idx) = node {
          let idx_usize = idx as usize;
          assert!(idx_usize < self.slots.len(), "idx out of bounds: {idx}");
          assert!(
            !seen[idx_usize],
            "timer appears in multiple buckets: idx={idx} level={level} bucket={bucket}"
          );
          seen[idx_usize] = true;

          let entry = self.slots[idx_usize]
            .entry
            .as_ref()
            .expect("bucket points to vacant slot");
          assert_eq!(entry.level as usize, level, "wrong level for idx={idx}");
          assert_eq!(entry.slot as usize, bucket, "wrong bucket for idx={idx}");
          assert_eq!(entry.prev, prev, "prev link mismatch for idx={idx}");

          computed_min = Some(match computed_min {
            Some(cur) if cur <= entry.deadline => cur,
            _ => entry.deadline,
          });

          prev = Some(idx);
          node = entry.next;
        }

        assert_eq!(prev, b.tail, "tail mismatch: level={level} bucket={bucket}");
        assert_eq!(
          b.min_deadline, computed_min,
          "min_deadline mismatch: level={level} bucket={bucket}"
        );
      }
    }

    for (idx, slot) in self.slots.iter().enumerate() {
      if slot.entry.is_some() {
        assert!(seen[idx], "entry not linked in wheel: idx={idx}");
      }
    }
  }
}

#[cfg(test)]
impl<T> TimerWheel<T> {
  pub(crate) fn debug_stats(&self) -> DebugStats {
    self.stats
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::clock::VirtualClock;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use std::task::{RawWaker, RawWakerVTable, Waker};
  use std::time::Duration;

  #[test]
  fn timers_fire_in_deadline_order() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    wheel.schedule(base + Duration::from_millis(10), 10);
    wheel.schedule(base + Duration::from_millis(1), 1);
    wheel.schedule(base + Duration::from_millis(5), 5);

    let mut fired = Vec::new();
    wheel.poll_expired(base + Duration::from_millis(10), |p| fired.push(p));
    assert_eq!(fired, vec![1, 5, 10]);
  }

  #[test]
  fn cancel_is_idempotent_and_removes_from_next_deadline() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    let k1 = wheel.schedule(base + Duration::from_millis(10), 1);
    let k2 = wheel.schedule(base + Duration::from_millis(5), 2);

    assert_eq!(wheel.next_deadline(), Some(base + Duration::from_millis(5)));

    assert_eq!(wheel.cancel(k2), Some(2));
    assert_eq!(wheel.cancel(k2), None);
    assert_eq!(
      wheel.next_deadline(),
      Some(base + Duration::from_millis(10))
    );

    assert_eq!(wheel.cancel(k1), Some(1));
    assert_eq!(wheel.next_deadline(), None);
  }

  #[test]
  fn multiple_timers_same_deadline_all_fire() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    let d = base + Duration::from_millis(7);
    wheel.schedule(d, 1);
    wheel.schedule(d, 2);
    wheel.schedule(d, 3);

    assert_eq!(wheel.next_deadline(), Some(d));

    let mut fired = Vec::new();
    wheel.poll_expired(d, |p| fired.push(p));
    fired.sort_unstable();
    assert_eq!(fired, vec![1, 2, 3]);
    assert_eq!(wheel.next_deadline(), None);
  }

  #[test]
  fn stale_key_cancel_does_not_cancel_reused_entry() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    let k1 = wheel.schedule(base + Duration::from_millis(1), 1);
    assert_eq!(wheel.cancel(k1), Some(1));

    // Next schedule should reuse the same arena index but bump the generation.
    let k2 = wheel.schedule(base + Duration::from_millis(2), 2);
    assert_ne!(k1, k2);
    assert_eq!(k1.index(), k2.index(), "expected slot reuse");

    // Stale key should be ignored.
    assert_eq!(wheel.cancel(k1), None);

    let mut fired = Vec::new();
    wheel.poll_expired(base + Duration::from_millis(2), |p| fired.push(p));
    assert_eq!(fired, vec![2]);
  }

  #[test]
  fn large_jump_advancement_fires_without_iterating_every_tick() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    let delay_ms = (1u64 << 31) - 1;
    let deadline = base + Duration::from_millis(delay_ms);
    wheel.schedule(deadline, 123);

    let mut fired = Vec::new();
    wheel.poll_expired(deadline, |p| fired.push(p));
    assert_eq!(fired, vec![123]);
    assert_eq!(wheel.next_deadline(), None);
  }

  #[test]
  fn tick_rounding_does_not_fire_before_deadline() {
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    wheel.schedule(base + Duration::from_nanos(1), 1);

    let mut fired = Vec::new();
    wheel.poll_expired(base, |p| fired.push(p));
    assert!(fired.is_empty());

    wheel.poll_expired(base + Duration::from_millis(1), |p| fired.push(p));
    assert_eq!(fired, vec![1]);
  }

  #[test]
  fn deterministic_stepping_works_with_virtual_clock() {
    let clock = VirtualClock::new();
    let base = Instant::now();
    let mut wheel = TimerWheel::<u64>::new_at(base);

    wheel.schedule(base + Duration::from_millis(10), 1);

    let mut fired = Vec::new();
    wheel.poll_expired(base + clock.now(), |p| fired.push(p));
    assert!(fired.is_empty());

    clock.advance(Duration::from_millis(9));
    wheel.poll_expired(base + clock.now(), |p| fired.push(p));
    assert!(fired.is_empty());

    clock.advance(Duration::from_millis(1));
    wheel.poll_expired(base + clock.now(), |p| fired.push(p));
    assert_eq!(fired, vec![1]);
  }

  #[test]
  fn fast_forward_single_max_i32_ms_timer_is_not_o_delta_ticks() {
    let base = Instant::now();
    let mut wheel: TimerWheel<Box<dyn FnOnce() + Send>> = TimerWheel::new_at(base);

    let fired = Arc::new(AtomicUsize::new(0));
    let fired_clone = Arc::clone(&fired);

    let deadline = base
      .checked_add(Duration::from_millis(2_147_483_647))
      .expect("deadline should fit in Instant range");
    wheel.schedule(deadline, Box::new(move || {
      fired_clone.fetch_add(1, Ordering::Relaxed);
    }));

    wheel.poll_expired(deadline, |cb| cb());

    assert_eq!(fired.load(Ordering::Relaxed), 1);

    let stats = wheel.debug_stats();
    assert!(stats.tick_steps < 10_000, "tick_steps too high: {stats:?}");
    assert!(stats.slot_visits < 10_000, "slot_visits too high: {stats:?}");
    assert!(stats.cascades < 1_000, "cascades too high: {stats:?}");
  }

  #[test]
  fn fast_forward_multiple_far_future_timers_is_not_o_delta_ticks() {
    let base = Instant::now();
    let mut wheel: TimerWheel<Box<dyn FnOnce() + Send>> = TimerWheel::new_at(base);

    let fired = Arc::new(AtomicUsize::new(0));
    let add = |wheel: &mut TimerWheel<Box<dyn FnOnce() + Send>>,
               fired: &Arc<AtomicUsize>,
               base: Instant,
               deadline_ms: u64| {
      let fired = Arc::clone(fired);
      let deadline = base
        .checked_add(Duration::from_millis(deadline_ms))
        .expect("deadline should fit in Instant range");
      wheel.schedule(deadline, Box::new(move || {
        fired.fetch_add(1, Ordering::Relaxed);
      }));
    };

    // Spread across multiple wheel levels:
    // - ~16 minutes (level 3)
    // - ~27 hours (level 4)
    // - ~24.8 days (level 5, max i32 ms)
    // - ~1 second (level 1) to ensure small timers don't degrade fast-forward.
    add(&mut wheel, &fired, base, 1_000);
    add(&mut wheel, &fired, base, 1_000_000);
    add(&mut wheel, &fired, base, 100_000_000);
    add(&mut wheel, &fired, base, 2_147_483_647);

    let poll_at = base
      .checked_add(Duration::from_millis(2_147_483_647))
      .expect("poll_at should fit in Instant range");
    wheel.poll_expired(poll_at, |cb| cb());

    assert_eq!(fired.load(Ordering::Relaxed), 4);

    let stats = wheel.debug_stats();
    assert!(stats.tick_steps < 10_000, "tick_steps too high: {stats:?}");
    assert!(stats.slot_visits < 10_000, "slot_visits too high: {stats:?}");
    assert!(stats.cascades < 1_000, "cascades too high: {stats:?}");
  }

  #[test]
  fn waking_works_with_custom_waker() {
    fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
      unsafe fn clone(data: *const ()) -> RawWaker {
        Arc::<AtomicUsize>::increment_strong_count(data as *const AtomicUsize);
        RawWaker::new(data, &VTABLE)
      }

      unsafe fn wake(data: *const ()) {
        let arc = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
        arc.fetch_add(1, Ordering::SeqCst);
      }

      unsafe fn wake_by_ref(data: *const ()) {
        let arc = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
        arc.fetch_add(1, Ordering::SeqCst);
        let _ = Arc::into_raw(arc);
      }

      unsafe fn drop(data: *const ()) {
        let _ = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
      }

      static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

      let raw = RawWaker::new(Arc::into_raw(counter) as *const (), &VTABLE);
      unsafe { Waker::from_raw(raw) }
    }

    let base = Instant::now();
    let counter = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(counter.clone());

    let mut wheel = TimerWheel::new_at(base);
    wheel.schedule(base + Duration::from_millis(1), WakeTask(waker));

    wheel.poll_expired(base + Duration::from_millis(1), |w| w.wake());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
  }
}
