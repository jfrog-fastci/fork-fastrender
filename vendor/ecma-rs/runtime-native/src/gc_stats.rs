use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::abi::RtGcStatsSnapshot;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOC_ARRAY_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_ARRAY_BYTES: AtomicUsize = AtomicUsize::new(0);
static GC_COLLECT_CALLS: AtomicU64 = AtomicU64::new(0);
static SAFEPOINT_CALLS: AtomicU64 = AtomicU64::new(0);
static WRITE_BARRIER_CALLS: AtomicU64 = AtomicU64::new(0);
static WRITE_BARRIER_RANGE_CALLS: AtomicU64 = AtomicU64::new(0);
static SET_YOUNG_RANGE_CALLS: AtomicU64 = AtomicU64::new(0);
static THREAD_INIT_CALLS: AtomicU64 = AtomicU64::new(0);
static THREAD_DEINIT_CALLS: AtomicU64 = AtomicU64::new(0);

pub fn record_alloc(size: usize) {
  ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
  ALLOC_BYTES.fetch_add(size, Ordering::Relaxed);
}

pub fn record_alloc_array(len: usize, elem_size: usize) {
  ALLOC_ARRAY_CALLS.fetch_add(1, Ordering::Relaxed);
  let bytes = len.saturating_mul(elem_size);
  ALLOC_ARRAY_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

pub fn record_gc_collect() {
  GC_COLLECT_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_safepoint() {
  SAFEPOINT_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_write_barrier() {
  WRITE_BARRIER_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_write_barrier_range() {
  WRITE_BARRIER_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_set_young_range() {
  SET_YOUNG_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_thread_init() {
  THREAD_INIT_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_thread_deinit() {
  THREAD_DEINIT_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub fn snapshot() -> RtGcStatsSnapshot {
  RtGcStatsSnapshot {
    alloc_calls: ALLOC_CALLS.load(Ordering::Relaxed),
    alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
    alloc_array_calls: ALLOC_ARRAY_CALLS.load(Ordering::Relaxed),
    alloc_array_bytes: ALLOC_ARRAY_BYTES.load(Ordering::Relaxed),
    gc_collect_calls: GC_COLLECT_CALLS.load(Ordering::Relaxed),
    safepoint_calls: SAFEPOINT_CALLS.load(Ordering::Relaxed),
    write_barrier_calls: WRITE_BARRIER_CALLS.load(Ordering::Relaxed),
    write_barrier_range_calls: WRITE_BARRIER_RANGE_CALLS.load(Ordering::Relaxed),
    set_young_range_calls: SET_YOUNG_RANGE_CALLS.load(Ordering::Relaxed),
    thread_init_calls: THREAD_INIT_CALLS.load(Ordering::Relaxed),
    thread_deinit_calls: THREAD_DEINIT_CALLS.load(Ordering::Relaxed),
  }
}

pub fn reset() {
  ALLOC_CALLS.store(0, Ordering::Relaxed);
  ALLOC_BYTES.store(0, Ordering::Relaxed);
  ALLOC_ARRAY_CALLS.store(0, Ordering::Relaxed);
  ALLOC_ARRAY_BYTES.store(0, Ordering::Relaxed);
  GC_COLLECT_CALLS.store(0, Ordering::Relaxed);
  SAFEPOINT_CALLS.store(0, Ordering::Relaxed);
  WRITE_BARRIER_CALLS.store(0, Ordering::Relaxed);
  WRITE_BARRIER_RANGE_CALLS.store(0, Ordering::Relaxed);
  SET_YOUNG_RANGE_CALLS.store(0, Ordering::Relaxed);
  THREAD_INIT_CALLS.store(0, Ordering::Relaxed);
  THREAD_DEINIT_CALLS.store(0, Ordering::Relaxed);
}

