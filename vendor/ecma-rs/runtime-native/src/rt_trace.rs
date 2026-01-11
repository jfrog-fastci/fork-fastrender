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
mod imp {
  use super::RtDebugCountersSnapshot;
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::sync::{Mutex, OnceLock};

  /// Per-thread counter storage.
  ///
  /// These counters are intentionally **not global atomics**: highly-contended global `fetch_add`
  /// operations can dominate benchmarks and make `rt-trace` unusably slow. Instead, each thread
  /// increments its own counters and the snapshot API aggregates them.
  struct ThreadCounters {
    steals_attempted: AtomicU64,
    steals_succeeded: AtomicU64,
    tasks_executed: AtomicU64,
    worker_park_count: AtomicU64,
    worker_unpark_count: AtomicU64,
    epoll_wakeups: AtomicU64,
  }

  impl ThreadCounters {
    fn new() -> Self {
      Self {
        steals_attempted: AtomicU64::new(0),
        steals_succeeded: AtomicU64::new(0),
        tasks_executed: AtomicU64::new(0),
        worker_park_count: AtomicU64::new(0),
        worker_unpark_count: AtomicU64::new(0),
        epoll_wakeups: AtomicU64::new(0),
      }
    }
  }

  static THREAD_COUNTERS: OnceLock<Mutex<Vec<&'static ThreadCounters>>> = OnceLock::new();

  fn registry() -> &'static Mutex<Vec<&'static ThreadCounters>> {
    THREAD_COUNTERS.get_or_init(|| Mutex::new(Vec::new()))
  }

  fn register_thread_counters(counters: &'static ThreadCounters) {
    // This is debug-only accounting; poisoning is not meaningful.
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    // Avoid duplicates (should be impossible since TLS init is one-shot, but keep it defensive).
    if reg.iter().any(|&c| std::ptr::eq(c, counters)) {
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

  #[inline(always)]
  pub(crate) fn steals_attempted_inc() {
    TLS_COUNTERS.with(|c| {
      c.steals_attempted.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn steals_succeeded_inc() {
    TLS_COUNTERS.with(|c| {
      c.steals_succeeded.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn tasks_executed_inc() {
    TLS_COUNTERS.with(|c| {
      c.tasks_executed.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn worker_park_inc() {
    TLS_COUNTERS.with(|c| {
      c.worker_park_count.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn worker_unpark_inc() {
    TLS_COUNTERS.with(|c| {
      c.worker_unpark_count.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn epoll_wakeups_inc() {
    TLS_COUNTERS.with(|c| {
      c.epoll_wakeups.fetch_add(1, Ordering::Relaxed);
    });
  }

  // Timer heap size is a shared gauge, not a per-thread counter.
  static TIMER_HEAP_SIZE: AtomicU64 = AtomicU64::new(0);

  #[inline(always)]
  pub(crate) fn timer_heap_inc_by(n: u64) {
    TIMER_HEAP_SIZE.fetch_add(n, Ordering::Relaxed);
  }

  #[inline(always)]
  pub(crate) fn timer_heap_dec_by(n: u64) {
    TIMER_HEAP_SIZE.fetch_sub(n, Ordering::Relaxed);
  }

  pub fn rt_debug_snapshot_counters() -> RtDebugCountersSnapshot {
    let mut snap = RtDebugCountersSnapshot::default();

    // This is debug-only accounting; poisoning is not meaningful.
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    for c in reg.iter() {
      snap.steals_attempted = snap
        .steals_attempted
        .wrapping_add(c.steals_attempted.load(Ordering::Relaxed));
      snap.steals_succeeded = snap
        .steals_succeeded
        .wrapping_add(c.steals_succeeded.load(Ordering::Relaxed));
      snap.tasks_executed = snap
        .tasks_executed
        .wrapping_add(c.tasks_executed.load(Ordering::Relaxed));
      snap.worker_park_count = snap
        .worker_park_count
        .wrapping_add(c.worker_park_count.load(Ordering::Relaxed));
      snap.worker_unpark_count = snap
        .worker_unpark_count
        .wrapping_add(c.worker_unpark_count.load(Ordering::Relaxed));
      snap.epoll_wakeups = snap
        .epoll_wakeups
        .wrapping_add(c.epoll_wakeups.load(Ordering::Relaxed));
    }

    snap.timer_heap_size = TIMER_HEAP_SIZE.load(Ordering::Relaxed);
    snap
  }
}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn steals_attempted_inc() {
  imp::steals_attempted_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn steals_attempted_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn steals_succeeded_inc() {
  imp::steals_succeeded_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn steals_succeeded_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn tasks_executed_inc() {
  imp::tasks_executed_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn tasks_executed_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn worker_park_inc() {
  imp::worker_park_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn worker_park_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn worker_unpark_inc() {
  imp::worker_unpark_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn worker_unpark_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn epoll_wakeups_inc() {
  imp::epoll_wakeups_inc();
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn epoll_wakeups_inc() {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn timer_heap_inc_by(n: u64) {
  imp::timer_heap_inc_by(n);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn timer_heap_inc_by(_n: u64) {}

#[cfg(feature = "rt-trace")]
#[inline(always)]
pub(crate) fn timer_heap_dec_by(n: u64) {
  imp::timer_heap_dec_by(n);
}

#[cfg(not(feature = "rt-trace"))]
#[inline(always)]
pub(crate) fn timer_heap_dec_by(_n: u64) {}

#[cfg(feature = "rt-trace")]
pub fn rt_debug_snapshot_counters() -> RtDebugCountersSnapshot {
  imp::rt_debug_snapshot_counters()
}

#[cfg(not(feature = "rt-trace"))]
pub fn rt_debug_snapshot_counters() -> RtDebugCountersSnapshot {
  RtDebugCountersSnapshot::default()
}
