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

/// Global GC/safepoint epoch (monotonically increasing).
///
/// # Semantics
/// - Even values mean "no stop-the-world GC requested".
/// - Odd values mean "stop-the-world GC requested".
///
/// This is exported as a stable, link-visible symbol so generated code can
/// inline the safepoint fast path as:
///
/// ```text
/// load RT_GC_EPOCH
/// test low bit; if set call rt_gc_safepoint()
/// ```
#[no_mangle]
pub static RT_GC_EPOCH: AtomicU64 = AtomicU64::new(0);

struct SafepointCoordinator {
  /// How many threads are currently blocked inside [`rt_gc_safepoint`]'s slow path.
  threads_waiting: AtomicUsize,

  cv_mutex: Mutex<()>,
  cv: Condvar,
}

impl SafepointCoordinator {
  fn new() -> Self {
    Self {
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
  RT_GC_EPOCH.load(Ordering::Acquire)
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
    let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if epoch & 1 == 0 {
      return;
    }
    guard = coord.cv.wait(guard).unwrap();
  }
}

/// Try to request a global stop-the-world safepoint.
///
/// Returns `Some(requested_epoch)` (odd) if this call successfully initiated the
/// stop-the-world request, or `None` if another request is already in progress.
pub fn rt_gc_try_request_stop_the_world() -> Option<u64> {
  let coord = coordinator();
  let mut cur = RT_GC_EPOCH.load(Ordering::Acquire);
  loop {
    if cur & 1 == 1 {
      return None;
    }
    let next = cur + 1;
    match RT_GC_EPOCH.compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire) {
      Ok(_) => {
        coord.notify_all();
        wake_all_gc_wakers();
        return Some(next);
      }
      Err(actual) => cur = actual,
    }
  }
}

/// Fast-path safepoint poll used by compiler-inserted statepoints and runtime loops.
///
/// - Fast path: one atomic load + branch.
/// - Slow path: publish the current epoch as "observed", then block until resumed.
#[inline(always)]
pub fn rt_gc_safepoint() {
  let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
  if epoch & 1 == 0 {
    return;
  }

  // Safety: `rt_gc_safepoint_slow` is part of the runtime and follows the
  // platform C ABI.
  unsafe {
    rt_gc_safepoint_slow(epoch);
  }
}

/// Fast-path check used by compiler-inserted loop backedge polls.
///
/// Returns `true` when a stop-the-world safepoint is currently requested.
///
/// This must remain a *leaf* (no calls) so codegen can mark it as
/// `"gc-leaf-function"` and keep the fast path free of statepoints.
#[inline(always)]
pub fn rt_gc_poll() -> bool {
  let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
  (epoch & 1) != 0
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
  while RT_GC_EPOCH.load(Ordering::Acquire) == requested_epoch {
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
  let mut cur = RT_GC_EPOCH.load(Ordering::Acquire);
  loop {
    if cur & 1 == 1 {
      panic!("GC stop-the-world requested while another stop is already in progress (epoch={cur})");
    }
    let next = cur + 1;
    match RT_GC_EPOCH.compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire) {
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
    let cur_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
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
  let stop_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
  if stop_epoch & 1 == 0 {
    return true;
  }

  let coordinator_id = registry::current_thread_id();

  let start = Instant::now();
  let mut guard = coord.cv_mutex.lock().unwrap();
  loop {
    // If the request was cancelled/resumed, treat as "stopped" for the caller.
    let cur_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
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
  let mut cur = RT_GC_EPOCH.load(Ordering::Acquire);
  loop {
    if cur & 1 == 0 {
      // Already resumed.
      return cur;
    }
    let next = cur + 1;
    match RT_GC_EPOCH.compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire) {
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

// -----------------------------------------------------------------------------
// Stop-the-world helper + root enumeration
// -----------------------------------------------------------------------------

/// Run `f` with the world stopped at a GC safepoint.
///
/// This is a convenience wrapper around:
/// - [`rt_gc_request_stop_the_world`]
/// - [`rt_gc_wait_for_world_stopped`]
/// - [`rt_gc_resume_world`]
pub fn with_world_stopped<T>(f: impl FnOnce(u64) -> T) -> T {
  let stop_epoch = rt_gc_request_stop_the_world();
  rt_gc_wait_for_world_stopped();

  struct ResumeOnDrop;
  impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
      // Always resume, even if `f` panics (tests) to avoid deadlocking other
      // threads.
      rt_gc_resume_world();
    }
  }
  let _guard = ResumeOnDrop;

  f(stop_epoch)
}

fn stackmaps_for_self() -> Option<&'static crate::StackMaps> {
  static STACKMAPS: OnceLock<Option<crate::StackMaps>> = OnceLock::new();
  STACKMAPS
    .get_or_init(|| {
      let bytes = crate::stackmaps_loader::load_stackmaps_from_self();
      if bytes.is_empty() {
        return None;
      }
      Some(crate::StackMaps::parse(bytes).expect("failed to parse .llvm_stackmaps"))
    })
    .as_ref()
}

/// Enumerate all GC root slots while the world is stopped.
///
/// Root sources (in order):
/// 1) Per-thread root scopes (runtime-native handle stack).
/// 2) Global/persistent roots registered via `rt_gc_register_root_slot` / `rt_gc_pin`.
/// 3) Stack roots described by LLVM statepoint stackmaps for each stopped mutator thread.
///
/// # Panics
/// Panics if `stop_epoch` is not an odd (stop-the-world) epoch.
pub fn for_each_root_slot_world_stopped(
  stop_epoch: u64,
  mut f: impl FnMut(*mut *mut u8),
) -> Result<(), crate::WalkError> {
  assert_eq!(
    stop_epoch & 1,
    1,
    "for_each_root_slot_world_stopped called with non-stop epoch {stop_epoch}"
  );

  // 1) Thread-local handle stacks.
  for thread in registry::all_threads() {
    thread.for_each_handle_slot(|slot| f(slot));
  }

  // 2) Global roots.
  crate::roots::global_root_registry().for_each_root_slot(|slot| f(slot));

  // 3) Stack roots from stackmaps.
  let Some(stackmaps) = stackmaps_for_self() else {
    return Ok(());
  };

  let coordinator_id = registry::current_thread_id();
  for thread in registry::all_threads() {
    if Some(thread.id()) == coordinator_id {
      continue;
    }
    if thread.is_parked() || thread.is_native_safe() {
      continue;
    }
    if thread.safepoint_epoch_observed() != stop_epoch {
      continue;
    }

    let ctx = thread
      .safepoint_context()
      .expect("stopped thread must have a published safepoint context");

    // SAFETY: The caller guarantees the world is stopped and the thread's stack
    // is stable to read.
    unsafe {
      crate::walk_gc_roots_from_fp(ctx.fp as u64, stackmaps, |slot_addr| {
        f(slot_addr as *mut *mut u8);
      })?;
    }
  }

  Ok(())
}
