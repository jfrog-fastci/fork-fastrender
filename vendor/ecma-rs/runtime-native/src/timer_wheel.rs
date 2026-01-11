use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TimerKey(u64);

// 64^6 ~= 6.87e10ms ~= 795 days, enough to cover JS' max timeout (2^31-1 ms).
const WHEEL_BITS: u32 = 6;
const WHEEL_SIZE: usize = 1 << WHEEL_BITS;
const WHEEL_MASK: u64 = (WHEEL_SIZE as u64) - 1;
const LEVELS: usize = 6;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DebugStats {
  /// How many times `poll_expired` moved the internal `current_tick` forward.
  pub tick_steps: u64,
  /// How many wheel slots were actually processed (expired or cascaded).
  pub slot_visits: u64,
  /// How many cascades were performed (higher-level bucket drained and reinserted).
  pub cascades: u64,
}

#[derive(Debug)]
struct TimerEntry<T> {
  key: TimerKey,
  deadline: Instant,
  deadline_tick: u64,
  payload: T,
}

#[derive(Debug)]
struct Level<T> {
  slots: [Vec<TimerEntry<T>>; WHEEL_SIZE],
  min_deadline: [Option<Instant>; WHEEL_SIZE],
  occupied: u64,
}

impl<T> Level<T> {
  fn new() -> Self {
    Self {
      slots: std::array::from_fn(|_| Vec::new()),
      min_deadline: [None; WHEEL_SIZE],
      occupied: 0,
    }
  }

  fn set_occupied(&mut self, slot: usize) {
    self.occupied |= 1u64 << slot;
  }

  fn clear_occupied(&mut self, slot: usize) {
    self.occupied &= !(1u64 << slot);
  }

  fn is_occupied(&self, slot: usize) -> bool {
    (self.occupied & (1u64 << slot)) != 0
  }
}

#[derive(Clone, Copy, Debug)]
struct Location {
  level: usize,
  slot: usize,
}

/// Timer scheduler with `TimerKey` cancellation.
///
/// Semantics:
/// - Timers never fire early: a timer scheduled for `deadline` only fires when polled with
///   `now >= deadline`.
/// - Cancellation is idempotent: canceling a stale/unknown key returns `None`.
/// - [`TimerWheel::next_deadline`] always reports the earliest scheduled deadline among active timers.
///
/// Implementation notes:
/// - Uses a hierarchical wheel (6 levels × 64 slots) with 1ms tick granularity.
/// - Per-level 64-bit occupancy masks allow `poll_expired(now)` to fast-forward across multi-day
///   jumps without per-tick iteration when the wheel is mostly empty.
pub struct TimerWheel<T> {
  start: Instant,
  current_tick: u64,
  next_key: u64,
  levels: [Level<T>; LEVELS],
  locations: HashMap<TimerKey, Location>,

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
    Self::new_with_start(Instant::now())
  }

  fn new_with_start(start: Instant) -> Self {
    Self {
      start,
      current_tick: 0,
      next_key: 0,
      levels: std::array::from_fn(|_| Level::new()),
      locations: HashMap::new(),
      #[cfg(test)]
      stats: DebugStats::default(),
    }
  }

  pub fn schedule(&mut self, deadline: Instant, payload: T) -> TimerKey {
    let key = TimerKey(self.next_key);
    self.next_key += 1;

    let entry = TimerEntry {
      key,
      deadline,
      deadline_tick: instant_to_tick_floor(self.start, deadline),
      payload,
    };
    self.insert_entry(entry);
    key
  }

  pub fn cancel(&mut self, key: TimerKey) -> Option<T> {
    let loc = self.locations.get(&key).copied()?;
    let bucket = &mut self.levels[loc.level].slots[loc.slot];

    let pos = bucket.iter().position(|entry| entry.key == key)?;
    let entry = bucket.swap_remove(pos);
    self.locations.remove(&key);

    self.recompute_slot_meta(loc.level, loc.slot);
    Some(entry.payload)
  }

  pub fn poll_expired<F: FnMut(T)>(&mut self, now: Instant, mut on_fired: F) {
    #[cfg(test)]
    {
      self.stats = DebugStats::default();
    }

    let target_tick = instant_to_tick_floor(self.start, now);
    if target_tick < self.current_tick {
      // Time went backwards (or `now` is before `start`). Preserve monotonicity.
      return;
    }

    // Expire any timers that were scheduled for <= current_tick after the last poll.
    self.expire_current_slot(now, &mut on_fired);

    self.advance_to(now, target_tick, &mut on_fired);
  }

  pub fn next_deadline(&self) -> Option<Instant> {
    let mut min_deadline: Option<Instant> = None;
    for level in &self.levels {
      let mut mask = level.occupied;
      while mask != 0 {
        let bit = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        let deadline = level.min_deadline[bit].expect("occupied slot must have a min deadline");
        min_deadline = Some(match min_deadline {
          Some(existing) => existing.min(deadline),
          None => deadline,
        });
      }
    }
    min_deadline
  }

  fn insert_entry(&mut self, entry: TimerEntry<T>) {
    let deadline_tick = entry.deadline_tick;
    if deadline_tick <= self.current_tick {
      let slot = (self.current_tick & WHEEL_MASK) as usize;
      self.push_entry(0, slot, entry);
      return;
    }

    let diff = deadline_tick - self.current_tick;
    let mut level = 0usize;
    let mut range: u64 = 1u64 << WHEEL_BITS; // 64^1
    while level + 1 < LEVELS && diff >= range {
      level += 1;
      range <<= WHEEL_BITS;
    }

    let slot = slot_for_tick(deadline_tick, level);
    self.push_entry(level, slot, entry);
  }

  fn push_entry(&mut self, level: usize, slot: usize, entry: TimerEntry<T>) {
    let key = entry.key;
    let deadline = entry.deadline;

    let lvl = &mut self.levels[level];
    lvl.slots[slot].push(entry);
    lvl.set_occupied(slot);
    lvl.min_deadline[slot] = Some(match lvl.min_deadline[slot] {
      Some(existing) => existing.min(deadline),
      None => deadline,
    });

    self.locations.insert(key, Location { level, slot });
  }

  fn recompute_slot_meta(&mut self, level: usize, slot: usize) {
    let lvl = &mut self.levels[level];
    if lvl.slots[slot].is_empty() {
      lvl.clear_occupied(slot);
      lvl.min_deadline[slot] = None;
      return;
    }

    lvl.set_occupied(slot);
    let mut min = lvl.slots[slot][0].deadline;
    for entry in &lvl.slots[slot][1..] {
      if entry.deadline < min {
        min = entry.deadline;
      }
    }
    lvl.min_deadline[slot] = Some(min);
  }

  fn advance_to<F: FnMut(T)>(&mut self, now: Instant, target_tick: u64, on_fired: &mut F) {
    while self.current_tick < target_tick {
      let Some(next_tick) = self.next_event_tick(target_tick) else {
        self.bump_tick(target_tick);
        return;
      };

      self.bump_tick(next_tick);
      self.process_cascades();
      self.expire_current_slot(now, on_fired);
    }
  }

  fn bump_tick(&mut self, new_tick: u64) {
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

  fn expire_current_slot<F: FnMut(T)>(&mut self, now: Instant, on_fired: &mut F) {
    let slot = (self.current_tick & WHEEL_MASK) as usize;

    #[cfg(test)]
    {
      self.stats.slot_visits += 1;
    }

    if !self.levels[0].is_occupied(slot) {
      return;
    }

    let mut bucket = std::mem::take(&mut self.levels[0].slots[slot]);
    self.levels[0].clear_occupied(slot);
    self.levels[0].min_deadline[slot] = None;

    for entry in bucket.drain(..) {
      if now >= entry.deadline {
        self.locations.remove(&entry.key);
        on_fired(entry.payload);
      } else {
        // Not due yet (sub-ms rounding, wheel wrap-around, or a host that polled early).
        self.insert_entry(entry);
      }
    }
  }

  fn process_cascades(&mut self) {
    for level in 1..LEVELS {
      let shift = WHEEL_BITS * level as u32;
      let step_mask = (1u64 << shift) - 1;
      if (self.current_tick & step_mask) != 0 {
        break;
      }

      let slot = slot_for_tick(self.current_tick, level);
      if !self.levels[level].is_occupied(slot) {
        continue;
      }

      #[cfg(test)]
      {
        self.stats.cascades += 1;
        self.stats.slot_visits += 1;
      }

      let mut bucket = std::mem::take(&mut self.levels[level].slots[slot]);
      self.levels[level].clear_occupied(slot);
      self.levels[level].min_deadline[slot] = None;

      for entry in bucket.drain(..) {
        self.insert_entry(entry);
      }
    }
  }

  fn next_event_tick(&self, target_tick: u64) -> Option<u64> {
    let mut best: Option<u64> = self.next_expiration_tick(target_tick);

    for level in 1..LEVELS {
      if let Some(tick) = self.next_cascade_tick(level, target_tick) {
        best = Some(match best {
          Some(existing) => existing.min(tick),
          None => tick,
        });
      }
    }

    best
  }

  fn next_expiration_tick(&self, target_tick: u64) -> Option<u64> {
    let mask = self.levels[0].occupied;
    if mask == 0 {
      return None;
    }

    let start = ((self.current_tick + 1) & WHEEL_MASK) as u32;
    let rotated = mask.rotate_right(start);
    let tz = rotated.trailing_zeros();
    if tz >= 64 {
      return None;
    }

    let tick = self.current_tick + 1 + tz as u64;
    (tick <= target_tick).then_some(tick)
  }

  fn next_cascade_tick(&self, level: usize, target_tick: u64) -> Option<u64> {
    let mask = self.levels[level].occupied;
    if mask == 0 {
      return None;
    }

    let shift = WHEEL_BITS * level as u32;
    let cur_step = self.current_tick >> shift;
    let start = ((cur_step + 1) & WHEEL_MASK) as u32;
    let rotated = mask.rotate_right(start);
    let tz = rotated.trailing_zeros();
    if tz >= 64 {
      return None;
    }

    let step = cur_step.checked_add(1 + tz as u64)?;
    let tick = step.checked_shl(shift)?;
    debug_assert!(tick > self.current_tick);
    (tick <= target_tick).then_some(tick)
  }
}

#[cfg(test)]
impl<T> TimerWheel<T> {
  pub(crate) fn debug_stats(&self) -> DebugStats {
    self.stats
  }
}

fn instant_to_tick_floor(start: Instant, now: Instant) -> u64 {
  let delta = now.checked_duration_since(start).unwrap_or_else(|| Duration::from_millis(0));
  delta.as_millis().min(u128::from(u64::MAX)) as u64
}

fn slot_for_tick(tick: u64, level: usize) -> usize {
  ((tick >> (WHEEL_BITS * level as u32)) & WHEEL_MASK) as usize
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;

  #[test]
  fn fast_forward_single_max_i32_ms_timer_is_not_o_delta_ticks() {
    let mut wheel: TimerWheel<Box<dyn FnOnce() + Send>> = TimerWheel::new();

    let fired = Arc::new(AtomicUsize::new(0));
    let fired_clone = Arc::clone(&fired);

    let deadline = Instant::now()
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
    let mut wheel: TimerWheel<Box<dyn FnOnce() + Send>> = TimerWheel::new();

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

    let base = Instant::now();

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
}

