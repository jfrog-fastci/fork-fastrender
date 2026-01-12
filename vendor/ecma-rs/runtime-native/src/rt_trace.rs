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
  use crate::sync::GcAwareMutex;
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::sync::OnceLock;

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

  static THREAD_COUNTERS: OnceLock<GcAwareMutex<Vec<&'static ThreadCounters>>> = OnceLock::new();

  fn registry() -> &'static GcAwareMutex<Vec<&'static ThreadCounters>> {
    THREAD_COUNTERS.get_or_init(|| GcAwareMutex::new(Vec::new()))
  }

  fn register_thread_counters(counters: &'static ThreadCounters) {
    let mut reg = registry().lock();
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
    let _ = TLS_COUNTERS.try_with(|c| {
      c.steals_attempted.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn steals_succeeded_inc() {
    let _ = TLS_COUNTERS.try_with(|c| {
      c.steals_succeeded.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn tasks_executed_inc() {
    let _ = TLS_COUNTERS.try_with(|c| {
      c.tasks_executed.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn worker_park_inc() {
    let _ = TLS_COUNTERS.try_with(|c| {
      c.worker_park_count.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn worker_unpark_inc() {
    let _ = TLS_COUNTERS.try_with(|c| {
      c.worker_unpark_count.fetch_add(1, Ordering::Relaxed);
    });
  }

  #[inline(always)]
  pub(crate) fn epoll_wakeups_inc() {
    let _ = TLS_COUNTERS.try_with(|c| {
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

    let reg = registry().lock();
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

  #[cfg(test)]
  mod tests {
    use super::*;
    use crate::threading;
    use crate::threading::ThreadKind;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn rt_trace_registry_lock_is_gc_aware() {
      let _rt = crate::test_util::TestRuntimeGuard::new();
      const TIMEOUT: Duration = Duration::from_secs(2);

      std::thread::scope(|scope| {
        // Thread A holds the registry lock.
        let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
        let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

        // Thread C attempts to snapshot while the lock is held.
        let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
        let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
        let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

        scope.spawn(move || {
          threading::register_current_thread(ThreadKind::Worker);
          let guard = registry().lock();
          a_locked_tx.send(()).unwrap();
          a_release_rx.recv().unwrap();
          drop(guard);

          // Cooperatively stop at the safepoint request.
          crate::rt_gc_safepoint();
          threading::unregister_current_thread();
        });

        a_locked_rx
          .recv_timeout(TIMEOUT)
          .expect("thread A should acquire the rt_trace registry lock");

        scope.spawn(move || {
          let id = threading::register_current_thread(ThreadKind::Worker);
          c_registered_tx.send(id).unwrap();
          c_start_rx.recv().unwrap();

          let _ = rt_debug_snapshot_counters();
          c_done_tx.send(()).unwrap();

          threading::unregister_current_thread();
        });

        let c_id = c_registered_rx
          .recv_timeout(TIMEOUT)
          .expect("thread C should register with the thread registry");

        // Ensure thread C is actively contending on the lock before starting STW.
        c_start_tx.send(()).unwrap();

        // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
        let start = Instant::now();
        loop {
          let mut native_safe = false;
          threading::registry::for_each_thread(|t| {
            if t.id() == c_id {
              native_safe = t.is_native_safe();
            }
          });

          if native_safe {
            break;
          }
          if start.elapsed() > TIMEOUT {
            panic!("thread C did not enter a GC-safe region while blocked on the rt_trace registry lock");
          }
          std::thread::yield_now();
        }

        // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
        let stop_epoch = crate::test_util::rt_gc_request_stop_the_world_for_tests(TIMEOUT);
        assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
        struct ResumeOnDrop;
        impl Drop for ResumeOnDrop {
          fn drop(&mut self) {
            crate::threading::safepoint::rt_gc_resume_world();
          }
        }
        let _resume = ResumeOnDrop;

        // Let thread A release the lock and reach the safepoint.
        a_release_tx.send(()).unwrap();

        assert!(
          crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
          "world failed to stop within timeout; rt_trace registry lock contention must not block STW"
        );

        // Resume the world so the contending snapshot can complete.
        crate::threading::safepoint::rt_gc_resume_world();

        c_done_rx
          .recv_timeout(TIMEOUT)
          .expect("snapshot should complete after world is resumed");
      });
    }
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
