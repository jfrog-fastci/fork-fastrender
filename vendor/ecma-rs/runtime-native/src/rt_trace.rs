#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtDebugCountersSnapshot {
  pub steals_attempted: u64,
  pub steals_succeeded: u64,
  pub tasks_executed: u64,
  pub worker_park_count: u64,
  pub worker_unpark_count: u64,
  pub epoll_wakeups: u64,
  pub timer_heap_size: u64,
}

#[cfg(feature = "rt-trace")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "rt-trace")]
static STEALS_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static STEALS_SUCCEEDED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static TASKS_EXECUTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static WORKER_PARK_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static WORKER_UNPARK_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static EPOLL_WAKEUPS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static TIMER_HEAP_SIZE: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn steals_attempted_inc() {
  STEALS_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn steals_attempted_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn steals_succeeded_inc() {
  STEALS_SUCCEEDED.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn steals_succeeded_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn tasks_executed_inc() {
  TASKS_EXECUTED.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn tasks_executed_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn worker_park_inc() {
  WORKER_PARK_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn worker_park_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn worker_unpark_inc() {
  WORKER_UNPARK_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn worker_unpark_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn epoll_wakeups_inc() {
  EPOLL_WAKEUPS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn epoll_wakeups_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn timer_heap_inc_by(n: u64) {
  TIMER_HEAP_SIZE.fetch_add(n, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn timer_heap_inc_by(_n: u64) {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn timer_heap_dec_by(n: u64) {
  TIMER_HEAP_SIZE.fetch_sub(n, Ordering::Relaxed);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn timer_heap_dec_by(_n: u64) {}

#[cfg(feature = "rt-trace")]
pub fn rt_debug_snapshot_counters() -> RtDebugCountersSnapshot {
  RtDebugCountersSnapshot {
    steals_attempted: STEALS_ATTEMPTED.load(Ordering::Relaxed),
    steals_succeeded: STEALS_SUCCEEDED.load(Ordering::Relaxed),
    tasks_executed: TASKS_EXECUTED.load(Ordering::Relaxed),
    worker_park_count: WORKER_PARK_COUNT.load(Ordering::Relaxed),
    worker_unpark_count: WORKER_UNPARK_COUNT.load(Ordering::Relaxed),
    epoll_wakeups: EPOLL_WAKEUPS.load(Ordering::Relaxed),
    timer_heap_size: TIMER_HEAP_SIZE.load(Ordering::Relaxed),
  }
}

#[cfg(not(feature = "rt-trace"))]
pub fn rt_debug_snapshot_counters() -> RtDebugCountersSnapshot {
  RtDebugCountersSnapshot::default()
}
