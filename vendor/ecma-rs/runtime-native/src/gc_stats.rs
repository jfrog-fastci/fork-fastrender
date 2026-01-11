use core::sync::atomic::{AtomicU64, Ordering};

use crate::abi::RtGcStatsSnapshot;

use parking_lot::Mutex;
use std::sync::OnceLock;

// NOTE: This module is only compiled when the `gc_stats` Cargo feature is enabled (see `lib.rs`).
//
// Keep the recording fast paths minimal: we avoid contended global atomics by using per-thread
// counters and aggregating them at snapshot time.

struct ThreadCounters {
  alloc_calls: AtomicU64,
  alloc_bytes: AtomicU64,
  alloc_array_calls: AtomicU64,
  alloc_array_bytes: AtomicU64,
  gc_collect_calls: AtomicU64,
  safepoint_calls: AtomicU64,
  write_barrier_calls_total: AtomicU64,
  write_barrier_range_calls: AtomicU64,
  write_barrier_old_young_hits: AtomicU64,
  set_young_range_calls: AtomicU64,
  thread_init_calls: AtomicU64,
  thread_deinit_calls: AtomicU64,
  remembered_objects_added: AtomicU64,
  remembered_objects_scanned_minor: AtomicU64,
  card_marks_total: AtomicU64,
  cards_scanned_minor: AtomicU64,
  cards_kept_after_rebuild: AtomicU64,
}

impl ThreadCounters {
  fn new() -> Self {
    Self {
      alloc_calls: AtomicU64::new(0),
      alloc_bytes: AtomicU64::new(0),
      alloc_array_calls: AtomicU64::new(0),
      alloc_array_bytes: AtomicU64::new(0),
      gc_collect_calls: AtomicU64::new(0),
      safepoint_calls: AtomicU64::new(0),
      write_barrier_calls_total: AtomicU64::new(0),
      write_barrier_range_calls: AtomicU64::new(0),
      write_barrier_old_young_hits: AtomicU64::new(0),
      set_young_range_calls: AtomicU64::new(0),
      thread_init_calls: AtomicU64::new(0),
      thread_deinit_calls: AtomicU64::new(0),
      remembered_objects_added: AtomicU64::new(0),
      remembered_objects_scanned_minor: AtomicU64::new(0),
      card_marks_total: AtomicU64::new(0),
      cards_scanned_minor: AtomicU64::new(0),
      cards_kept_after_rebuild: AtomicU64::new(0),
    }
  }
}

static THREAD_COUNTERS: OnceLock<Mutex<Vec<&'static ThreadCounters>>> = OnceLock::new();

#[inline]
fn registry() -> &'static Mutex<Vec<&'static ThreadCounters>> {
  THREAD_COUNTERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register_thread_counters(counters: &'static ThreadCounters) {
  let mut reg = registry().lock();
  // Avoid duplicates (should be impossible since TLS init is one-shot, but keep it defensive).
  if reg.iter().any(|&c| core::ptr::eq(c, counters)) {
    return;
  }
  reg.push(counters);
}

thread_local! {
  static TLS_COUNTERS: &'static ThreadCounters = {
    let counters = Box::leak(Box::new(ThreadCounters::new()));
    register_thread_counters(counters);
    counters
  };
}

pub fn record_alloc(size: usize) {
  TLS_COUNTERS.with(|c| {
    c.alloc_calls.fetch_add(1, Ordering::Relaxed);
    c.alloc_bytes.fetch_add(size as u64, Ordering::Relaxed);
  });
}

pub fn record_alloc_array(len: usize, elem_size: usize) {
  let bytes = (len as u64).saturating_mul(elem_size as u64);
  TLS_COUNTERS.with(|c| {
    c.alloc_array_calls.fetch_add(1, Ordering::Relaxed);
    c.alloc_array_bytes.fetch_add(bytes, Ordering::Relaxed);
  });
}

pub fn record_gc_collect() {
  TLS_COUNTERS.with(|c| {
    c.gc_collect_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_safepoint() {
  TLS_COUNTERS.with(|c| {
    c.safepoint_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_write_barrier() {
  TLS_COUNTERS.with(|c| {
    c.write_barrier_calls_total.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_write_barrier_range() {
  TLS_COUNTERS.with(|c| {
    c.write_barrier_range_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_write_barrier_old_young_hit() {
  TLS_COUNTERS.with(|c| {
    c.write_barrier_old_young_hits.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_set_young_range() {
  TLS_COUNTERS.with(|c| {
    c.set_young_range_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_thread_init() {
  TLS_COUNTERS.with(|c| {
    c.thread_init_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_thread_deinit() {
  TLS_COUNTERS.with(|c| {
    c.thread_deinit_calls.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_remembered_object_added() {
  TLS_COUNTERS.with(|c| {
    c.remembered_objects_added.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_remembered_object_scanned_minor() {
  TLS_COUNTERS.with(|c| {
    c.remembered_objects_scanned_minor.fetch_add(1, Ordering::Relaxed);
  });
}

pub fn record_card_marks(count: u64) {
  if count == 0 {
    return;
  }
  TLS_COUNTERS.with(|c| {
    c.card_marks_total.fetch_add(count, Ordering::Relaxed);
  });
}

pub fn record_cards_scanned_minor(count: u64) {
  if count == 0 {
    return;
  }
  TLS_COUNTERS.with(|c| {
    c.cards_scanned_minor.fetch_add(count, Ordering::Relaxed);
  });
}

// This counter is reserved for future sticky card-table rebuild logic. The current minor GC clears
// all cards (no young objects remain after full nursery evacuation), so no callsites record this
// yet.
#[allow(dead_code)]
pub fn record_cards_kept_after_rebuild(count: u64) {
  if count == 0 {
    return;
  }
  TLS_COUNTERS.with(|c| {
    c.cards_kept_after_rebuild.fetch_add(count, Ordering::Relaxed);
  });
}

pub fn snapshot() -> RtGcStatsSnapshot {
  let mut snap = RtGcStatsSnapshot::default();

  let reg = registry().lock();
  for c in reg.iter() {
    snap.alloc_calls = snap.alloc_calls.wrapping_add(c.alloc_calls.load(Ordering::Relaxed));
    snap.alloc_bytes = snap.alloc_bytes.wrapping_add(c.alloc_bytes.load(Ordering::Relaxed));
    snap.alloc_array_calls =
      snap.alloc_array_calls.wrapping_add(c.alloc_array_calls.load(Ordering::Relaxed));
    snap.alloc_array_bytes =
      snap.alloc_array_bytes.wrapping_add(c.alloc_array_bytes.load(Ordering::Relaxed));
    snap.gc_collect_calls =
      snap.gc_collect_calls.wrapping_add(c.gc_collect_calls.load(Ordering::Relaxed));
    snap.safepoint_calls = snap.safepoint_calls.wrapping_add(c.safepoint_calls.load(Ordering::Relaxed));
    snap.write_barrier_calls_total =
      snap.write_barrier_calls_total.wrapping_add(c.write_barrier_calls_total.load(Ordering::Relaxed));
    snap.write_barrier_range_calls =
      snap.write_barrier_range_calls.wrapping_add(c.write_barrier_range_calls.load(Ordering::Relaxed));
    snap.write_barrier_old_young_hits =
      snap.write_barrier_old_young_hits.wrapping_add(c.write_barrier_old_young_hits.load(Ordering::Relaxed));
    snap.set_young_range_calls =
      snap.set_young_range_calls.wrapping_add(c.set_young_range_calls.load(Ordering::Relaxed));
    snap.thread_init_calls =
      snap.thread_init_calls.wrapping_add(c.thread_init_calls.load(Ordering::Relaxed));
    snap.thread_deinit_calls =
      snap.thread_deinit_calls.wrapping_add(c.thread_deinit_calls.load(Ordering::Relaxed));
    snap.remembered_objects_added =
      snap.remembered_objects_added.wrapping_add(c.remembered_objects_added.load(Ordering::Relaxed));
    snap.remembered_objects_scanned_minor =
      snap.remembered_objects_scanned_minor.wrapping_add(c.remembered_objects_scanned_minor.load(Ordering::Relaxed));
    snap.card_marks_total =
      snap.card_marks_total.wrapping_add(c.card_marks_total.load(Ordering::Relaxed));
    snap.cards_scanned_minor =
      snap.cards_scanned_minor.wrapping_add(c.cards_scanned_minor.load(Ordering::Relaxed));
    snap.cards_kept_after_rebuild =
      snap.cards_kept_after_rebuild.wrapping_add(c.cards_kept_after_rebuild.load(Ordering::Relaxed));
  }

  snap
}

pub fn reset() {
  let reg = registry().lock();
  for c in reg.iter() {
    c.alloc_calls.store(0, Ordering::Relaxed);
    c.alloc_bytes.store(0, Ordering::Relaxed);
    c.alloc_array_calls.store(0, Ordering::Relaxed);
    c.alloc_array_bytes.store(0, Ordering::Relaxed);
    c.gc_collect_calls.store(0, Ordering::Relaxed);
    c.safepoint_calls.store(0, Ordering::Relaxed);
    c.write_barrier_calls_total.store(0, Ordering::Relaxed);
    c.write_barrier_range_calls.store(0, Ordering::Relaxed);
    c.write_barrier_old_young_hits.store(0, Ordering::Relaxed);
    c.set_young_range_calls.store(0, Ordering::Relaxed);
    c.thread_init_calls.store(0, Ordering::Relaxed);
    c.thread_deinit_calls.store(0, Ordering::Relaxed);
    c.remembered_objects_added.store(0, Ordering::Relaxed);
    c.remembered_objects_scanned_minor.store(0, Ordering::Relaxed);
    c.card_marks_total.store(0, Ordering::Relaxed);
    c.cards_scanned_minor.store(0, Ordering::Relaxed);
    c.cards_kept_after_rebuild.store(0, Ordering::Relaxed);
  }
}
