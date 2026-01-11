use crate::arch::SafepointContext;
use crate::threading::registry;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

extern "C" {
  fn rt_gc_safepoint_slow(requested_epoch: u64);
}

struct SafepointCoordinator {
  /// Global GC/safepoint epoch.
  ///
  /// Even epochs mean "no stop-the-world GC requested".
  /// Odd epochs mean "stop-the-world GC requested".
  epoch: AtomicU64,

  /// How many threads are currently blocked inside [`rt_gc_safepoint`]'s slow path.
  threads_waiting: AtomicUsize,

  cv_mutex: Mutex<()>,
  cv: Condvar,
}

impl SafepointCoordinator {
  fn new() -> Self {
    Self {
      epoch: AtomicU64::new(0),
      threads_waiting: AtomicUsize::new(0),
      cv_mutex: Mutex::new(()),
      cv: Condvar::new(),
    }
  }

  fn notify_all(&self) {
    self.cv.notify_all();
  }
}

static COORDINATOR: OnceLock<SafepointCoordinator> = OnceLock::new();
static GC_WAKERS: OnceLock<Mutex<Vec<fn()>>> = OnceLock::new();

fn coordinator() -> &'static SafepointCoordinator {
  COORDINATOR.get_or_init(SafepointCoordinator::new)
}

fn gc_wakers() -> &'static Mutex<Vec<fn()>> {
  GC_WAKERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a callback that should be invoked whenever the GC requests a
/// stop-the-world safepoint.
///
/// This is used to wake threads blocked in external wait primitives (e.g.
/// `epoll_wait` inside `rt_async_poll`).
pub fn register_gc_waker(waker: fn()) {
  let mut wakers = gc_wakers().lock().unwrap();
  if wakers.iter().any(|&w| w as usize == waker as usize) {
    return;
  }
  wakers.push(waker);
}

fn wake_all_gc_wakers() {
  let wakers = { gc_wakers().lock().unwrap().clone() };
  for w in wakers {
    w();
  }
}

/// Current global safepoint epoch (monotonically increasing).
pub(crate) fn current_epoch() -> u64 {
  coordinator().epoch.load(Ordering::Acquire)
}

/// Notify any threads waiting for the world to stop that some observable state
/// has changed (thread arrived at a safepoint, parked/unparked, registered, ...).
pub(crate) fn notify_state_change() {
  coordinator().notify_all();
}

/// Block the current thread until any in-progress stop-the-world request is resumed.
///
/// This is used by GC-safe ("native") region transitions: a thread must not leave
/// a GC-safe region and resume mutator execution while a stop-the-world GC is
/// active.
pub(crate) fn wait_while_stop_the_world() {
  let coord = coordinator();
  let mut guard = coord.cv_mutex.lock().unwrap();
  loop {
    let epoch = coord.epoch.load(Ordering::Acquire);
    if epoch & 1 == 0 {
      return;
    }
    guard = coord.cv.wait(guard).unwrap();
  }
}

/// Fast-path safepoint poll used by compiler-inserted statepoints and runtime loops.
///
/// - Fast path: one atomic load + branch.
/// - Slow path: publish the current epoch as "observed", then block until resumed.
#[inline(always)]
pub fn rt_gc_safepoint() {
  let epoch = coordinator().epoch.load(Ordering::Acquire);
  if epoch & 1 == 0 {
    return;
  }

  // Safety: `rt_gc_safepoint_slow` is part of the runtime and follows the
  // platform C ABI.
  unsafe {
    rt_gc_safepoint_slow(epoch);
  }
}

/// Rust implementation of the safepoint slow path.
///
/// This is called via the architecture-specific assembly shim `rt_gc_safepoint_slow`, which
/// captures the caller's stack pointer / frame pointer / return address before any Rust
/// prologue can mutate them.
#[no_mangle]
#[cold]
extern "C" fn rt_gc_safepoint_slow_impl(requested_epoch: u64, ctx: *const SafepointContext) {
  // Safety: the assembly wrapper passes a valid pointer to an initialized
  // `SafepointContext` on its stack.
  let ctx = unsafe { *ctx };

  registry::set_current_thread_safepoint_context(ctx);
  // Publish that we've observed the stop-the-world request.
  registry::set_current_thread_safepoint_epoch_observed(requested_epoch);
  notify_state_change();

  let coord = coordinator();
  coord.threads_waiting.fetch_add(1, Ordering::SeqCst);
  let mut guard = coord.cv_mutex.lock().unwrap();
  while coord.epoch.load(Ordering::Acquire) == requested_epoch {
    guard = coord.cv.wait(guard).unwrap();
  }
  drop(guard);
  coord.threads_waiting.fetch_sub(1, Ordering::SeqCst);
}

/// Request a global stop-the-world safepoint.
///
/// Returns the requested (odd) epoch.
pub fn rt_gc_request_stop_the_world() -> u64 {
  let coord = coordinator();
  let mut cur = coord.epoch.load(Ordering::Acquire);
  loop {
    if cur & 1 == 1 {
      panic!("GC stop-the-world requested while another stop is already in progress (epoch={cur})");
    }
    let next = cur + 1;
    match coord
      .epoch
      .compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire)
    {
      Ok(_) => {
        coord.notify_all();
        wake_all_gc_wakers();
        return next;
      }
      Err(actual) => cur = actual,
    }
  }
}

/// Wait until all registered threads have acknowledged the current stop-the-world request.
///
/// Threads marked as `parked` are treated as already quiescent.
pub fn rt_gc_wait_for_world_stopped() {
  let coord = coordinator();

  let coordinator_id = registry::current_thread_id();

  let mut guard = coord.cv_mutex.lock().unwrap();
  loop {
    let cur_epoch = coord.epoch.load(Ordering::Acquire);
    if cur_epoch & 1 == 0 {
      return;
    }

    if world_stopped(cur_epoch, coordinator_id) {
      return;
    }

    guard = coord.cv.wait(guard).unwrap();
  }
}

/// Like [`rt_gc_wait_for_world_stopped`], but with a timeout.
pub fn rt_gc_wait_for_world_stopped_timeout(timeout: Duration) -> bool {
  let coord = coordinator();
  let stop_epoch = coord.epoch.load(Ordering::Acquire);
  if stop_epoch & 1 == 0 {
    return true;
  }

  let coordinator_id = registry::current_thread_id();

  let start = Instant::now();
  let mut guard = coord.cv_mutex.lock().unwrap();
  loop {
    // If the request was cancelled/resumed, treat as "stopped" for the caller.
    let cur_epoch = coord.epoch.load(Ordering::Acquire);
    if cur_epoch & 1 == 0 {
      return true;
    }
    debug_assert_eq!(cur_epoch, stop_epoch, "nested GC requests are not supported");

    if world_stopped(stop_epoch, coordinator_id) {
      return true;
    }

    let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
      return false;
    };
    if remaining.is_zero() {
      return false;
    }

    let (g, wait_res) = coord.cv.wait_timeout(guard, remaining).unwrap();
    guard = g;
    if wait_res.timed_out() && !world_stopped(stop_epoch, coordinator_id) {
      return false;
    }
  }
}

fn world_stopped(stop_epoch: u64, coordinator_id: Option<registry::ThreadId>) -> bool {
  for thread in registry::all_threads() {
    if Some(thread.id()) == coordinator_id {
      continue;
    }
    if thread.is_parked() {
      continue;
    }
    if thread.is_native_safe() {
      debug_assert!(
        thread
          .safepoint_context()
          .map(|ctx| ctx.ip != 0)
          .unwrap_or(false),
        "thread {:?} is NativeSafe but has no published safepoint ip",
        thread.id()
      );
      continue;
    }
    if thread.safepoint_epoch_observed() == stop_epoch {
      continue;
    }
    return false;
  }
  true
}

/// Resume all threads after stop-the-world.
///
/// Returns the new (even) epoch.
pub fn rt_gc_resume_world() -> u64 {
  let coord = coordinator();
  let mut cur = coord.epoch.load(Ordering::Acquire);
  loop {
    if cur & 1 == 0 {
      // Already resumed.
      return cur;
    }
    let next = cur + 1;
    match coord
      .epoch
      .compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire)
    {
      Ok(_) => {
        coord.notify_all();
        return next;
      }
      Err(actual) => cur = actual,
    }
  }
}

/// Number of threads currently blocked in the safepoint slow path.
pub fn threads_waiting_at_safepoint() -> usize {
  coordinator().threads_waiting.load(Ordering::Acquire)
}
