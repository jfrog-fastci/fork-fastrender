use core::sync::atomic::{AtomicU64, Ordering};

use crate::abi::RtGcStatsSnapshot;

// NOTE: This module is only compiled when the `gc_stats` Cargo feature is enabled (see `lib.rs`).
// Keep the recording fast-paths minimal and lock-free: prefer `Relaxed` atomic ops.

pub struct GcStats {
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

impl GcStats {
  const fn new() -> Self {
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

  #[inline]
  fn snapshot(&self) -> RtGcStatsSnapshot {
    RtGcStatsSnapshot {
      alloc_calls: self.alloc_calls.load(Ordering::Relaxed),
      alloc_bytes: self.alloc_bytes.load(Ordering::Relaxed),
      alloc_array_calls: self.alloc_array_calls.load(Ordering::Relaxed),
      alloc_array_bytes: self.alloc_array_bytes.load(Ordering::Relaxed),
      gc_collect_calls: self.gc_collect_calls.load(Ordering::Relaxed),
      safepoint_calls: self.safepoint_calls.load(Ordering::Relaxed),
      write_barrier_calls_total: self.write_barrier_calls_total.load(Ordering::Relaxed),
      write_barrier_range_calls: self.write_barrier_range_calls.load(Ordering::Relaxed),
      write_barrier_old_young_hits: self.write_barrier_old_young_hits.load(Ordering::Relaxed),
      set_young_range_calls: self.set_young_range_calls.load(Ordering::Relaxed),
      thread_init_calls: self.thread_init_calls.load(Ordering::Relaxed),
      thread_deinit_calls: self.thread_deinit_calls.load(Ordering::Relaxed),
      remembered_objects_added: self.remembered_objects_added.load(Ordering::Relaxed),
      remembered_objects_scanned_minor: self.remembered_objects_scanned_minor.load(Ordering::Relaxed),
      card_marks_total: self.card_marks_total.load(Ordering::Relaxed),
      cards_scanned_minor: self.cards_scanned_minor.load(Ordering::Relaxed),
      cards_kept_after_rebuild: self.cards_kept_after_rebuild.load(Ordering::Relaxed),
    }
  }

  fn reset(&self) {
    self.alloc_calls.store(0, Ordering::Relaxed);
    self.alloc_bytes.store(0, Ordering::Relaxed);
    self.alloc_array_calls.store(0, Ordering::Relaxed);
    self.alloc_array_bytes.store(0, Ordering::Relaxed);
    self.gc_collect_calls.store(0, Ordering::Relaxed);
    self.safepoint_calls.store(0, Ordering::Relaxed);
    self.write_barrier_calls_total.store(0, Ordering::Relaxed);
    self.write_barrier_range_calls.store(0, Ordering::Relaxed);
    self.write_barrier_old_young_hits.store(0, Ordering::Relaxed);
    self.set_young_range_calls.store(0, Ordering::Relaxed);
    self.thread_init_calls.store(0, Ordering::Relaxed);
    self.thread_deinit_calls.store(0, Ordering::Relaxed);
    self.remembered_objects_added.store(0, Ordering::Relaxed);
    self.remembered_objects_scanned_minor.store(0, Ordering::Relaxed);
    self.card_marks_total.store(0, Ordering::Relaxed);
    self.cards_scanned_minor.store(0, Ordering::Relaxed);
    self.cards_kept_after_rebuild.store(0, Ordering::Relaxed);
  }
}

static GC_STATS: GcStats = GcStats::new();

pub fn record_alloc(size: usize) {
  GC_STATS.alloc_calls.fetch_add(1, Ordering::Relaxed);
  GC_STATS.alloc_bytes.fetch_add(size as u64, Ordering::Relaxed);
}

pub fn record_alloc_array(len: usize, elem_size: usize) {
  GC_STATS.alloc_array_calls.fetch_add(1, Ordering::Relaxed);
  let bytes = (len as u64).saturating_mul(elem_size as u64);
  GC_STATS.alloc_array_bytes.fetch_add(bytes, Ordering::Relaxed);
}

pub fn record_gc_collect() {
  GC_STATS.gc_collect_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_safepoint() {
  GC_STATS.safepoint_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_write_barrier() {
  GC_STATS.write_barrier_calls_total.fetch_add(1, Ordering::Relaxed);
}

pub fn record_write_barrier_range() {
  GC_STATS.write_barrier_range_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_write_barrier_old_young_hit() {
  GC_STATS.write_barrier_old_young_hits.fetch_add(1, Ordering::Relaxed);
}

pub fn record_set_young_range() {
  GC_STATS.set_young_range_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_thread_init() {
  GC_STATS.thread_init_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_thread_deinit() {
  GC_STATS.thread_deinit_calls.fetch_add(1, Ordering::Relaxed);
}

pub fn record_remembered_object_added() {
  GC_STATS.remembered_objects_added.fetch_add(1, Ordering::Relaxed);
}

pub fn record_remembered_object_scanned_minor() {
  GC_STATS.remembered_objects_scanned_minor.fetch_add(1, Ordering::Relaxed);
}

pub fn record_card_marks(count: u64) {
  if count == 0 {
    return;
  }
  GC_STATS.card_marks_total.fetch_add(count, Ordering::Relaxed);
}

pub fn record_cards_scanned_minor(count: u64) {
  if count == 0 {
    return;
  }
  GC_STATS.cards_scanned_minor.fetch_add(count, Ordering::Relaxed);
}

// This counter is reserved for future sticky card-table rebuild logic. The current minor GC clears
// all cards (no young objects remain after full nursery evacuation), so no callsites record this
// yet.
#[allow(dead_code)]
pub fn record_cards_kept_after_rebuild(count: u64) {
  if count == 0 {
    return;
  }
  GC_STATS.cards_kept_after_rebuild.fetch_add(count, Ordering::Relaxed);
}

pub fn snapshot() -> RtGcStatsSnapshot {
  GC_STATS.snapshot()
}

pub fn reset() {
  GC_STATS.reset();
}
