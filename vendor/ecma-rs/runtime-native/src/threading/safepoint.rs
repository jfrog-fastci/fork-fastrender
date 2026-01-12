use crate::arch::SafepointContext;
use crate::gc::RootSet;
use crate::gc_roots::RelocPair;
use crate::sync::GcAwareMutex;
use crate::threading::registry;
use parking_lot::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use std::cell::Cell;

#[cfg(target_arch = "aarch64")]
use crate::arch::aarch64::RegContext;

#[cfg(not(target_arch = "aarch64"))]
extern "C" {
  #[cfg(not(target_arch = "aarch64"))]
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
/// test low bit; if set call rt_gc_safepoint_slow(observed_epoch)
/// ```
#[no_mangle]
pub static RT_GC_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
  Gc,
  Test,
}

thread_local! {
  static IN_STOP_THE_WORLD: Cell<bool> = const { Cell::new(false) };
  /// Stop-the-world epoch this thread initiated as coordinator, if any.
  ///
  /// This is used by GC-aware synchronization primitives to distinguish the GC coordinator from
  /// mutator threads while the world is stopped.
  ///
  /// The higher-level [`stop_the_world`] helper sets `IN_STOP_THE_WORLD` (a boolean), but lower-level
  /// stop-the-world APIs (`rt_gc_try_request_stop_the_world`, `rt_gc_request_stop_the_world`) are
  /// also used directly by runtime code and tests. Those call sites need a way to identify the
  /// coordinator thread without relying on `IN_STOP_THE_WORLD`.
  static STW_COORDINATOR_EPOCH: Cell<u64> = const { Cell::new(0) };
  /// Optional override for safepoint-context fixup.
  ///
  /// Some runtime entrypoints wrap their bodies in `std::panic::catch_unwind` to
  /// avoid unwinding across the C ABI. The sysroot's `catch_unwind` implementation
  /// is not guaranteed to maintain a valid frame-pointer chain (it may repurpose
  /// the FP register for general computation).
  ///
  /// When a safepoint poll happens inside such a `catch_unwind` closure, the
  /// captured `SafepointContext.fp` still points at the closure frame, but the
  /// saved caller FP inside that frame record may be junk. In that situation, the
  /// normal "walk outward until we find a stackmapped return address" heuristic
  /// can fail.
  ///
  /// Runtime entrypoints that know a *stable* starting FP outside the `catch_unwind`
  /// may temporarily set this override while executing a safepoint poll. If the
  /// fixup logic fails to walk from the captured FP, it will fall back to this
  /// override value.
  static SAFEPOINT_FIXUP_START_FP: Cell<u64> = const { Cell::new(0) };
}

/// Returns `true` if the current thread is the stop-the-world coordinator for `epoch`.
///
/// This is intended for internal use by GC-aware locks. The coordinator must be able to acquire
/// locks while the world is stopped; mutator threads must not proceed holding such locks during a
/// stop-the-world epoch, or root enumeration can deadlock.
#[inline]
pub(crate) fn is_stop_the_world_coordinator(epoch: u64) -> bool {
  if epoch & 1 == 0 {
    return false;
  }
  STW_COORDINATOR_EPOCH
    .try_with(|cell| cell.get() == epoch)
    .unwrap_or(false)
}

/// Returns whether the current thread is acting as the stop-the-world coordinator.
///
/// This is used by GC-aware synchronization primitives: the coordinator is allowed to acquire
/// locks while a stop-the-world epoch is active (it must do so to enumerate roots), while mutator
/// threads must not resume execution during that epoch.
pub(crate) fn in_stop_the_world() -> bool {
  IN_STOP_THE_WORLD.try_with(|flag| flag.get()).unwrap_or(false)
}

/// RAII guard that marks the current thread as the stop-the-world coordinator.
///
/// This is a lightweight internal hook used by coordinator-side helpers (e.g. `safepoint::with_world_stopped`)
/// so GC-aware locks can distinguish coordinator code from mutator code.
pub(crate) struct StopTheWorldCoordinatorGuard {
  prev: bool,
}

impl Drop for StopTheWorldCoordinatorGuard {
  fn drop(&mut self) {
    let _ = IN_STOP_THE_WORLD.try_with(|flag| flag.set(self.prev));
  }
}

pub(crate) fn enter_stop_the_world_coordinator() -> StopTheWorldCoordinatorGuard {
  let prev = IN_STOP_THE_WORLD
    .try_with(|flag| {
      let prev = flag.get();
      flag.set(true);
      prev
    })
    .unwrap_or(false);
  StopTheWorldCoordinatorGuard { prev }
}

pub(crate) fn with_safepoint_fixup_start_fp<R>(fp: u64, f: impl FnOnce() -> R) -> R {
  let mut f = Some(f);
  match SAFEPOINT_FIXUP_START_FP.try_with(|cell| {
    let prev = cell.replace(fp);
    struct Restore<'a> {
      cell: &'a Cell<u64>,
      prev: u64,
    }
    impl Drop for Restore<'_> {
      fn drop(&mut self) {
        self.cell.set(self.prev);
      }
    }
    let _restore = Restore { cell, prev };
    f.take().unwrap()()
  }) {
    Ok(res) => res,
    Err(_) => f.take().unwrap()(),
  }
}
struct SafepointCoordinator {
  /// How many threads are currently blocked inside [`rt_gc_safepoint`]'s slow path.
  threads_waiting: AtomicUsize,

  gc_lock: GcAwareMutex<()>,

  cv_mutex: Mutex<()>,
  cv: Condvar,
}

impl SafepointCoordinator {
  fn new() -> Self {
    Self {
      threads_waiting: AtomicUsize::new(0),
      gc_lock: GcAwareMutex::new(()),
      cv_mutex: Mutex::new(()),
      cv: Condvar::new(),
    }
  }

  fn notify_all_locked(&self, _guard: &std::sync::MutexGuard<'_, ()>) {
    self.cv.notify_all();
  }

  fn notify_all(&self) {
    let guard = self.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
    self.notify_all_locked(&guard);
  }
}

static COORDINATOR: OnceLock<SafepointCoordinator> = OnceLock::new();
static GC_WAKERS: OnceLock<GcAwareMutex<Vec<fn()>>> = OnceLock::new();

fn coordinator() -> &'static SafepointCoordinator {
  COORDINATOR.get_or_init(SafepointCoordinator::new)
}

fn gc_wakers() -> &'static GcAwareMutex<Vec<fn()>> {
  GC_WAKERS.get_or_init(|| GcAwareMutex::new(Vec::new()))
}

/// Register a callback that should be invoked whenever the GC requests a
/// stop-the-world safepoint.
///
/// This is used to wake threads blocked in external wait primitives (e.g.
/// the async reactor wait syscall inside `rt_async_poll` / `rt_async_wait`).
pub fn register_gc_waker(waker: fn()) {
  // `gc_wakers` protects a best-effort list of wake callbacks; poisoning is not meaningful here
  // (a panic while registering wakers shouldn't permanently prevent GC coordination from waking
  // blocked threads).
  let mut wakers = gc_wakers().lock();
  if wakers.iter().any(|&w| w as usize == waker as usize) {
    return;
  }
  wakers.push(waker);
}

fn wake_all_gc_wakers() {
  // Avoid allocating during GC coordination; copy out one function pointer at a time.
  let mut idx = 0usize;
  loop {
    let Some(waker) = gc_wakers().lock_for_gc().get(idx).copied()
    else {
      break;
    };
    waker();
    idx += 1;
  }
}

/// Current global safepoint epoch (monotonically increasing).
#[doc(hidden)]
pub fn current_epoch() -> u64 {
  RT_GC_EPOCH.load(Ordering::Acquire)
}

/// Notify any threads waiting for the world to stop that some observable state
/// has changed (thread arrived at a safepoint, parked/unparked, registered, ...).
pub(crate) fn notify_state_change() {
  // Avoid lost wakeups by synchronizing notifications with the mutex used by
  // waiters.
  //
  // Condition variables do not "queue" notifications; if a notifier calls
  // `notify_all` just before a waiter transitions into `wait`, the waiter may
  // sleep indefinitely even though the condition has already become true.
  //
  // We use `cv_mutex` as the canonical coordination lock: all waiters hold it
  // while checking stop-the-world conditions, and all notifiers briefly acquire
  // it before waking waiters. This ensures notifies can't race with the
  // check→sleep transition.
  coordinator().notify_all();
}

/// Block the current thread until any in-progress stop-the-world request is resumed.
///
/// This is used by GC-safe ("native") region transitions: a thread must not leave
/// a GC-safe region and resume mutator execution while a stop-the-world GC is
/// active.
pub(crate) fn wait_while_stop_the_world() {
  let coord = coordinator();
  // `cv_mutex` only exists to synchronize notify/wait transitions; poisoning is irrelevant.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if epoch & 1 == 0 {
      return;
    }
    guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
  }
}

/// Lock held by the GC coordinator while the world is stopped and thread contexts are being read.
pub(crate) fn gc_world_lock() -> MutexGuard<'static, ()> {
  coordinator().gc_lock.lock()
}

/// Block the current thread while the global safepoint epoch remains equal to `expected_epoch`.
///
/// This is the primitive used by the safepoint slow path to park a thread until
/// the coordinator resumes the world.
#[cfg(target_arch = "x86_64")]
pub(crate) fn wait_while_epoch_is(expected_epoch: u64) {
  let coord = coordinator();
  // `cv_mutex` only exists to synchronize notify/wait transitions; poisoning is irrelevant.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  while RT_GC_EPOCH.load(Ordering::Acquire) == expected_epoch {
    guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
  }
}

/// Try to request a global stop-the-world safepoint.
///
/// Returns `Some(requested_epoch)` (odd) if this call successfully initiated the
/// stop-the-world request, or `None` if another request is already in progress.
pub fn rt_gc_try_request_stop_the_world() -> Option<u64> {
  let coord = coordinator();
  // Serialize stop-the-world requests with mutators unregistering from the thread registry.
  let _gc_guard = gc_world_lock();

  // Synchronize the epoch transition with any threads waiting on `cv`.
  let guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  let mut cur = RT_GC_EPOCH.load(Ordering::Acquire);
  loop {
    if cur & 1 == 1 {
      return None;
    }
    let next = cur + 1;
    match RT_GC_EPOCH.compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire) {
      Ok(_) => {
        // Mark this thread as the active STW coordinator so GC-safe transitions and GC-aware locks
        // can distinguish it from mutators.
        let _ = IN_STOP_THE_WORLD.try_with(|flag| flag.set(true));
        let _ = STW_COORDINATOR_EPOCH.try_with(|cell| cell.set(next));
        coord.notify_all_locked(&guard);
        drop(guard);
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
#[cfg(not(target_arch = "aarch64"))]
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

/// AArch64 safepoint poll.
///
/// On AArch64 `rt_gc_safepoint` tail-branches into an assembly stub
/// (`arch/aarch64/rt_gc_safepoint*.S`) so we can capture the caller FP/LR and spill the register
/// file before any Rust code runs.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn rt_gc_safepoint() {
  extern "C" {
    #[link_name = "runtime_native_gc_safepoint_asm"]
    fn rt_gc_safepoint_asm();
  }

  // Safety: `rt_gc_safepoint_asm` is a runtime entrypoint that follows the C ABI.
  unsafe {
    rt_gc_safepoint_asm();
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

pub(crate) fn fixup_safepoint_context_to_nearest_managed(
  mut ctx: SafepointContext,
  stackmaps: Option<&crate::stackmaps::StackMaps>,
) -> SafepointContext {
  // If the thread entered the safepoint slow path from within runtime-native code (rather than from
  // a managed `gc.safepoint_poll` callsite), the captured return address (`ctx.ip`) points into
  // runtime code and will not have an LLVM stackmap record.
  //
  // In that situation the top managed frame is suspended at the callsite into the *outermost*
  // runtime frame. Recover that managed callsite cursor by walking the frame-pointer chain outward
  // until we find a return address present in stackmaps, and publish that cursor as the thread's
  // safepoint context so stackmap-based root enumeration can still succeed.
  let Some(stackmaps) = stackmaps else {
    return ctx;
  };

  if stackmaps.lookup(ctx.ip as u64).is_some() {
    return ctx;
  }

  let start_fp = ctx.fp as u64;
  let mut cursor = crate::stackwalk::find_nearest_managed_cursor(start_fp, stackmaps);
  if cursor.is_none() {
    cursor = SAFEPOINT_FIXUP_START_FP
      .try_with(|cell| {
        let override_fp = cell.get();
        if override_fp != 0 && override_fp != start_fp {
          crate::stackwalk::find_nearest_managed_cursor(override_fp, stackmaps)
        } else {
          None
        }
      })
      .unwrap_or(None);
  }

  let Some(cursor) = cursor else {
    return ctx;
  };

  let sp_callsite = cursor.sp.unwrap_or(0);
  #[cfg(target_arch = "x86_64")]
  let sp_entry = sp_callsite.saturating_sub(crate::arch::WORD_SIZE as u64);
  #[cfg(not(target_arch = "x86_64"))]
  let sp_entry = sp_callsite;

  let regs = ctx.regs;
  ctx = SafepointContext {
    sp_entry: sp_entry as usize,
    sp: sp_callsite as usize,
    fp: cursor.fp as usize,
    ip: cursor.pc as usize,
    regs,
  };

  ctx
}

/// Rust implementation of the safepoint slow path.
///
/// This is called via the architecture-specific assembly shim `rt_gc_safepoint_slow`, which
/// captures the caller's stack pointer / frame pointer / return address before any Rust
/// prologue can mutate them.
#[no_mangle]
#[cold]
extern "C" fn runtime_native_gc_safepoint_slow_impl(requested_epoch: u64, ctx: *const SafepointContext) {
  crate::ffi::abort_on_panic(|| runtime_native_gc_safepoint_slow_impl_inner(requested_epoch, ctx));
}

#[cold]
fn runtime_native_gc_safepoint_slow_impl_inner(requested_epoch: u64, ctx: *const SafepointContext) {
  // Safety: the assembly wrapper passes a valid pointer to an initialized
  // `SafepointContext` on its stack.
  let mut ctx = unsafe { *ctx };
  ctx = fixup_safepoint_context_to_nearest_managed(ctx, crate::stackmap::try_stackmaps());

  registry::set_current_thread_safepoint_context(ctx);
  // Publish that we've observed the stop-the-world request.
  registry::set_current_thread_safepoint_epoch_observed(requested_epoch);
  notify_state_change();

  if requested_epoch & 1 == 0 {
    return;
  }

  let coord = coordinator();
  coord.threads_waiting.fetch_add(1, Ordering::SeqCst);
  // `cv_mutex` is only used to synchronize notify/wait transitions; poisoning is not meaningful.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if epoch & 1 == 0 {
      registry::set_current_thread_safepoint_epoch_observed(epoch);
      break;
    }
    guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
  }
  // Notify after releasing the mutex to avoid self-deadlocking with
  // `notify_state_change`'s synchronization.
  drop(guard);
  notify_state_change();
  coord.threads_waiting.fetch_sub(1, Ordering::SeqCst);
}

pub fn stop_the_world<F, R>(reason: StopReason, f: F) -> R
where
  F: FnOnce() -> R,
{
  let already_in_stw = IN_STOP_THE_WORLD
    .try_with(|flag| {
      if flag.get() {
        true
      } else {
        flag.set(true);
        false
      }
    })
    .unwrap_or(false);
  if already_in_stw {
    panic!("stop_the_world is not re-entrant");
  }
  struct ClearFlag;
  impl Drop for ClearFlag {
    fn drop(&mut self) {
      let _ = IN_STOP_THE_WORLD.try_with(|flag| flag.set(false));
    }
  }
  let _clear = ClearFlag;

  let coord = coordinator();
  let gc_guard = coord.gc_lock.lock_for_gc();

  let coordinator_id = registry::current_thread_id();

  // Acquire the coordination mutex up-front so the epoch transition + first
  // notification can't race with waiters transitioning into `wait`.
  let cv_guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  let cur = RT_GC_EPOCH.load(Ordering::Acquire);
  if cur & 1 == 1 {
    drop(cv_guard);
    drop(gc_guard);
    panic!("stop_the_world({reason:?}) requested while already in progress (epoch={cur})");
  }

  let stop_epoch = cur + 1;
  let _ = STW_COORDINATOR_EPOCH.try_with(|cell| cell.set(stop_epoch));
  RT_GC_EPOCH.store(stop_epoch, Ordering::Release);
  coord.notify_all_locked(&cv_guard);

  let mut expected_threads: Vec<_> = Vec::new();
  registry::for_each_thread(|thread| {
    if Some(thread.id()) == coordinator_id {
      return;
    }
    if thread.is_parked() || thread.is_native_safe() {
      return;
    }
    expected_threads.push(thread.clone());
  });
  drop(cv_guard);

  wake_all_gc_wakers();

  let deadline = cfg!(debug_assertions).then(|| Instant::now() + Duration::from_secs(5));
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  let stopped = loop {
    let all_stopped = expected_threads.iter().all(|t| {
      if t.is_detached() {
        return true;
      }
      if t.is_parked() {
        return true;
      }
      if t.is_native_safe() {
        debug_assert!(
          t.safepoint_context()
            .map(|ctx| ctx.ip != 0)
            .unwrap_or(false),
          "thread {:?} is NativeSafe but has no published safepoint ip",
          t.id()
        );
        return true;
      }
      t.safepoint_epoch_observed() == stop_epoch
    });
    if all_stopped {
      break true;
    }
    if let Some(deadline) = deadline {
      let now = Instant::now();
      if now >= deadline {
        break false;
      }
      let remaining = deadline - now;
      let (g, _) = coord
        .cv
        .wait_timeout(guard, remaining)
        .unwrap_or_else(|e| e.into_inner());
      guard = g;
    } else {
      guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
    }
  };
  drop(guard);
  if !stopped {
    let resume_epoch = stop_epoch + 1;
    {
      let guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
      RT_GC_EPOCH.store(resume_epoch, Ordering::Release);
      coord.notify_all_locked(&guard);
    }
    drop(gc_guard);
    panic!("stop_the_world({reason:?}) timed out waiting for threads to park");
  }

  let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

  let resume_epoch = stop_epoch + 1;
  {
    let guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
    RT_GC_EPOCH.store(resume_epoch, Ordering::Release);
    coord.notify_all_locked(&guard);
  }
  let _ = STW_COORDINATOR_EPOCH.try_with(|cell| cell.set(0));

  let deadline = cfg!(debug_assertions).then(|| Instant::now() + Duration::from_secs(5));
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  let resumed = loop {
    let all_resumed = expected_threads.iter().all(|t| {
      if t.is_detached() {
        return true;
      }
      if t.is_parked() {
        return true;
      }
      if t.is_native_safe() {
        debug_assert!(
          t.safepoint_context()
            .map(|ctx| ctx.ip != 0)
            .unwrap_or(false),
          "thread {:?} is NativeSafe but has no published safepoint ip",
          t.id()
        );
        return true;
      }
      t.safepoint_epoch_observed() == resume_epoch
    });
    if all_resumed {
      break true;
    }
    if let Some(deadline) = deadline {
      let now = Instant::now();
      if now >= deadline {
        break false;
      }
      let remaining = deadline - now;
      let (g, _) = coord
        .cv
        .wait_timeout(guard, remaining)
        .unwrap_or_else(|e| e.into_inner());
      guard = g;
    } else {
      guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
    }
  };
  drop(guard);
  if !resumed {
    drop(gc_guard);
    panic!("stop_the_world({reason:?}) timed out waiting for threads to resume");
  }

  drop(gc_guard);

  match res {
    Ok(v) => v,
    Err(p) => std::panic::resume_unwind(p),
  }
}

/// Rust implementation of the AArch64 assembly safepoint stub.
///
/// This is called from `arch/aarch64/rt_gc_safepoint.S` after the stub:
/// - detects a stop-the-world request,
/// - spills registers into a [`RegContext`] on its stack,
/// - captures the caller's frame pointer and return address at the safepoint
///   callsite.
#[cfg(target_arch = "aarch64")]
#[no_mangle]
#[cold]
extern "C" fn rt_gc_safepoint_impl(caller_fp: u64, caller_pc: u64, regs: *mut RegContext) {
  crate::ffi::abort_on_panic(|| rt_gc_safepoint_impl_inner(caller_fp, caller_pc, regs));
}

#[cfg(target_arch = "aarch64")]
#[cold]
fn rt_gc_safepoint_impl_inner(caller_fp: u64, caller_pc: u64, regs: *mut RegContext) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_safepoint();
  // A spurious slow-path entry can happen if the GC request was resumed between
  // the assembly fast-path check and this call. Re-check the epoch and return.
  let requested_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
  if requested_epoch & 1 == 0 {
    return;
  }

  // Safety: the assembly wrapper passes a valid pointer to an initialized
  // `RegContext` living on its stack for the duration of this call.
  let sp_entry = unsafe { (*regs).sp } as usize;

  // Publish the callsite state (FP + return PC) so the coordinator can match
  // `.llvm_stackmaps` records and enumerate stack roots for this thread.
  let ctx = SafepointContext {
    sp_entry,
    sp: sp_entry,
    fp: caller_fp as usize,
    ip: caller_pc as usize,
    regs: regs.cast(),
  };
  let ctx = fixup_safepoint_context_to_nearest_managed(ctx, crate::stackmap::try_stackmaps());

  registry::set_current_thread_safepoint_context(ctx);
  registry::set_current_thread_safepoint_epoch_observed(requested_epoch);
  notify_state_change();

  let coord = coordinator();
  coord.threads_waiting.fetch_add(1, Ordering::SeqCst);
  // `cv_mutex` is only used to synchronize notify/wait transitions; poisoning is not meaningful.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    let epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if epoch & 1 == 0 {
      registry::set_current_thread_safepoint_epoch_observed(epoch);
      break;
    }
    guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
  }
  // Notify after releasing the mutex to avoid self-deadlocking with
  // `notify_state_change`'s synchronization.
  drop(guard);
  notify_state_change();
  coord.threads_waiting.fetch_sub(1, Ordering::SeqCst);

  // Note: `regs` lives on the assembly stub's stack. While the thread is stopped,
  // the GC can update this register file in-place (e.g. to relocate register-located
  // stackmap roots), and the assembly epilogue will restore the updated registers
  // before returning to managed code.
}

/// Request a global stop-the-world safepoint.
///
/// Returns the requested (odd) epoch.
pub fn rt_gc_request_stop_the_world() -> u64 {
  rt_gc_try_request_stop_the_world().unwrap_or_else(|| {
    let cur = RT_GC_EPOCH.load(Ordering::Acquire);
    panic!("GC stop-the-world requested while another stop is already in progress (epoch={cur})");
  })
}

/// Wait until all registered threads have acknowledged the current stop-the-world request.
///
/// Threads marked as `parked` (or in a GC-safe/"native" region) are treated as already quiescent.
pub fn rt_gc_wait_for_world_stopped() {
  let coord = coordinator();

  let coordinator_id = registry::current_thread_id();

  // `cv_mutex` is only used to synchronize notify/wait transitions; poisoning is not meaningful.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    let cur_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if cur_epoch & 1 == 0 {
      return;
    }

    if world_stopped(cur_epoch, coordinator_id) {
      return;
    }

    guard = coord.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
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
  // `cv_mutex` is only used to synchronize notify/wait transitions; poisoning is not meaningful.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    // If the request was cancelled/resumed, treat as "stopped" for the caller.
    let cur_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    if cur_epoch & 1 == 0 {
      return true;
    }
    debug_assert_eq!(
      cur_epoch, stop_epoch,
      "nested GC requests are not supported"
    );

    if world_stopped(stop_epoch, coordinator_id) {
      return true;
    }

    let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
      return false;
    };
    if remaining.is_zero() {
      return false;
    }

    let (g, wait_res) = coord
      .cv
      .wait_timeout(guard, remaining)
      .unwrap_or_else(|e| e.into_inner());
    guard = g;
    if wait_res.timed_out() && !world_stopped(stop_epoch, coordinator_id) {
      return false;
    }
  }
}

/// Wait until all registered threads have observed the current (even) safepoint epoch.
///
/// This is a *post-resume* barrier used to ensure stop-the-world cycles don't overlap across epochs:
/// if a new stop-the-world request begins before threads have returned from the previous safepoint
/// slow path, those threads may miss the brief resumed epoch and remain blocked across requests.
pub fn rt_gc_wait_for_world_resumed_timeout(timeout: Duration) -> bool {
  let coord = coordinator();
  let resume_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
  if resume_epoch & 1 == 1 {
    // Still stopped (or another request has started). Callers should only use
    // this after resuming.
    return false;
  }

  let coordinator_id = registry::current_thread_id();

  let start = Instant::now();
  // `cv_mutex` is only used to synchronize notify/wait transitions; poisoning is not meaningful.
  let mut guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  loop {
    let cur_epoch = RT_GC_EPOCH.load(Ordering::Acquire);
    debug_assert_eq!(cur_epoch, resume_epoch, "nested GC requests are not supported");

    if world_stopped(resume_epoch, coordinator_id) {
      return true;
    }

    let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
      return false;
    };
    if remaining.is_zero() {
      return false;
    }

    let (g, wait_res) = coord
      .cv
      .wait_timeout(guard, remaining)
      .unwrap_or_else(|e| e.into_inner());
    guard = g;
    if wait_res.timed_out() && !world_stopped(resume_epoch, coordinator_id) {
      return false;
    }
  }
}

fn world_stopped(stop_epoch: u64, coordinator_id: Option<registry::ThreadId>) -> bool {
  let mut ok = true;
  registry::for_each_thread(|thread| {
    if !ok {
      return;
    }
    if Some(thread.id()) == coordinator_id {
      return;
    }
    // Threads that are detaching/unregistered should not be awaited: they will not
    // make further safepoint progress, and the drop path removes them from the
    // registry asynchronously w.r.t. the coordinator loop.
    if thread.is_detached() {
      return;
    }
    if thread.is_parked() {
      return;
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
      return;
    }
    if thread.safepoint_epoch_observed() == stop_epoch {
      return;
    }
    ok = false;
  });
  ok
}

/// Resume all threads after stop-the-world.
///
/// Returns the new (even) epoch.
pub fn rt_gc_resume_world() -> u64 {
  let coord = coordinator();
  // Synchronize the epoch transition with any threads waiting on `cv`.
  let guard = coord.cv_mutex.lock().unwrap_or_else(|e| e.into_inner());
  let mut cur = RT_GC_EPOCH.load(Ordering::Acquire);
  loop {
    if cur & 1 == 0 {
      // Already resumed.
      let _ = IN_STOP_THE_WORLD.try_with(|flag| flag.set(false));
      return cur;
    }
    let next = cur + 1;
    match RT_GC_EPOCH.compare_exchange(cur, next, Ordering::SeqCst, Ordering::Acquire) {
      Ok(_) => {
        let _ = STW_COORDINATOR_EPOCH.try_with(|cell| {
          if cell.get() == cur {
            cell.set(0);
          }
        });
        coord.notify_all_locked(&guard);
        let _ = IN_STOP_THE_WORLD.try_with(|flag| flag.set(false));
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

/// Best-effort diagnostics for stop-the-world timeouts.
///
/// This is intended to be called by the GC coordinator when
/// [`rt_gc_wait_for_world_stopped_timeout`] returns `false`.
pub fn dump_stop_the_world_timeout(stop_epoch: u64, timeout: Duration) {
  eprintln!(
    "runtime-native: stop-the-world timed out (epoch={stop_epoch}, timeout={timeout:?}, threads_waiting={})",
    threads_waiting_at_safepoint()
  );

  let counts = registry::thread_counts();
  eprintln!(
    "  registered threads: total={} main={} worker={} io={} external={}",
    counts.total, counts.main, counts.worker, counts.io, counts.external
  );

  let coordinator_id = registry::current_thread_id();
  for thread in registry::all_threads() {
    let role = if Some(thread.id()) == coordinator_id {
      "coordinator"
    } else {
      "mutator"
    };

    let status = if thread.is_parked() {
      "parked"
    } else if thread.is_native_safe() {
      "native_safe"
    } else if thread.safepoint_epoch_observed() == stop_epoch {
      "at_safepoint"
    } else {
      "RUNNING (not yet stopped)"
    };

    let handle_slots = thread.handle_stack_len();
    let bounds = thread.stack_bounds();
    let ctx = thread.safepoint_context();

    match (bounds, ctx) {
      (Some(bounds), Some(ctx)) => {
        eprintln!(
          "  thread id={} os_tid={} kind={:?} role={role} status={status} observed_epoch={} handle_slots={handle_slots} stack=[{:#x},{:#x}) ctx_ip={:#x} ctx_fp={:#x} ctx_sp={:#x} ctx_sp_entry={:#x}",
          thread.id().get(),
          thread.os_thread_id(),
          thread.kind(),
          thread.safepoint_epoch_observed(),
          bounds.lo,
          bounds.hi,
          ctx.ip,
          ctx.fp,
          ctx.sp,
          ctx.sp_entry,
        );
      }
      (Some(bounds), None) => {
        eprintln!(
          "  thread id={} os_tid={} kind={:?} role={role} status={status} observed_epoch={} handle_slots={handle_slots} stack=[{:#x},{:#x}) ctx=<none>",
          thread.id().get(),
          thread.os_thread_id(),
          thread.kind(),
          thread.safepoint_epoch_observed(),
          bounds.lo,
          bounds.hi,
        );
      }
      (None, Some(ctx)) => {
        eprintln!(
          "  thread id={} os_tid={} kind={:?} role={role} status={status} observed_epoch={} handle_slots={handle_slots} stack=<unknown> ctx_ip={:#x} ctx_fp={:#x} ctx_sp={:#x} ctx_sp_entry={:#x}",
          thread.id().get(),
          thread.os_thread_id(),
          thread.kind(),
          thread.safepoint_epoch_observed(),
          ctx.ip,
          ctx.fp,
          ctx.sp,
          ctx.sp_entry,
        );
      }
      (None, None) => {
        eprintln!(
          "  thread id={} os_tid={} kind={:?} role={role} status={status} observed_epoch={} handle_slots={handle_slots} stack=<unknown> ctx=<none>",
          thread.id().get(),
          thread.os_thread_id(),
          thread.kind(),
          thread.safepoint_epoch_observed(),
        );
      }
    }
  }
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
  crate::stackmap::try_stackmaps()
}

/// Enumerate all GC relocation pairs while the world is stopped.
///
/// This is the relocation/update-phase counterpart to
/// [`for_each_root_slot_world_stopped`]:
///
/// - Root enumeration for tracing visits **base** root slots.
/// - Relocation pair enumeration yields `(base_slot, derived_slot)` so a moving
///   GC can update derived (interior) pointers while preserving offsets.
///
/// Root sources (in order):
/// 1) Per-thread root scopes (runtime-native handle stack): `(slot, slot)`.
/// 2) Per-thread shadow stacks (runtime-native Rust handle scopes): `(slot, slot)`.
/// 3) Global/persistent roots registered via `rt_gc_register_root_slot` / `rt_gc_pin`:
///    `(slot, slot)`.
/// 4) Persistent roots stored in the global handle table (`roots::PersistentHandleTable`):
///    `(slot, slot)`.
/// 5) Stack roots described by LLVM statepoint stackmaps for each thread that is either:
///    - has observed `stop_epoch` (published `safepoint_epoch_observed == stop_epoch`), or
///    - is in a GC-safe ("NativeSafe") region with a published safepoint context:
///    `(base_slot, derived_slot)` pairs.
///
/// # Panics
/// Panics if `stop_epoch` is not an odd (stop-the-world) epoch.
pub fn for_each_reloc_pair_world_stopped(
  stop_epoch: u64,
  f: impl FnMut(crate::gc_roots::RelocPair),
) -> Result<(), crate::scan::ScanError> {
  for_each_reloc_pair_world_stopped_with_stackmaps(stop_epoch, stackmaps_for_self(), f)
}

/// Like [`for_each_reloc_pair_world_stopped`], but uses the provided `stackmaps` blob instead of
/// loading `.llvm_stackmaps` from the current binary.
///
/// This exists to keep tests deterministic: integration tests are built as standalone binaries and
/// do not necessarily contain stackmap metadata.
#[doc(hidden)]
pub fn for_each_reloc_pair_world_stopped_with_stackmaps(
  stop_epoch: u64,
  stackmaps: Option<&crate::StackMaps>,
  mut f: impl FnMut(crate::gc_roots::RelocPair),
) -> Result<(), crate::scan::ScanError> {
  assert_eq!(
    stop_epoch & 1,
    1,
    "for_each_reloc_pair_world_stopped called with non-stop epoch {stop_epoch}"
  );

  // 1) Thread-local handle stacks.
  registry::for_each_thread(|thread| {
    thread.for_each_handle_slot(|slot| {
      let slot = slot.cast::<u8>();
      f(crate::gc_roots::RelocPair {
        base_slot: crate::statepoints::RootSlot::StackAddr(slot),
        derived_slot: crate::statepoints::RootSlot::StackAddr(slot),
      })
    });
  });

  // 2) Thread-local shadow stacks.
  registry::for_each_thread(|thread| {
    let mut stack = thread.shadow_stack();
    stack.for_each_root_slot(&mut |slot| {
      let slot = slot.cast::<u8>();
      f(crate::gc_roots::RelocPair {
        base_slot: crate::statepoints::RootSlot::StackAddr(slot),
        derived_slot: crate::statepoints::RootSlot::StackAddr(slot),
      })
    });
  });

  // 3) Global roots.
  crate::roots::global_root_registry().for_each_root_slot(|slot| {
    let slot = slot.cast::<u8>();
    f(crate::gc_roots::RelocPair {
      base_slot: crate::statepoints::RootSlot::StackAddr(slot),
      derived_slot: crate::statepoints::RootSlot::StackAddr(slot),
    })
  });

  // 4) Persistent handle-table roots.
  crate::roots::global_persistent_handle_table().for_each_root_slot(|slot| {
    let slot = slot.cast::<u8>();
    f(crate::gc_roots::RelocPair {
      base_slot: crate::statepoints::RootSlot::StackAddr(slot),
      derived_slot: crate::statepoints::RootSlot::StackAddr(slot),
    })
  });

  // 5) Stack roots from stackmaps.
  let Some(stackmaps) = stackmaps else {
    return Ok(());
  };

  let coordinator_id = registry::current_thread_id();
  registry::try_for_each_thread(|thread| -> Result<(), crate::scan::ScanError> {
    if thread.is_detached() {
      return Ok(());
    }
    if thread.is_parked() {
      return Ok(());
    }

    let is_coordinator = Some(thread.id()) == coordinator_id;
    if is_coordinator && thread.is_native_safe() {
      return Ok(());
    }
    if !thread.is_native_safe() && thread.safepoint_epoch_observed() != stop_epoch {
      return Ok(());
    }

    let ctx = if is_coordinator {
      let Some(ctx) = thread.safepoint_context() else {
        return Ok(());
      };
      ctx
    } else {
      thread
        .safepoint_context()
        .expect("thread eligible for stack root enumeration must have a published safepoint context")
    };

    let stack_bounds = thread
      .stack_bounds()
      .and_then(|b| crate::stackwalk::StackBounds::new(b.lo as u64, b.hi as u64).ok());

    let thread_id = thread.id().get();
    let os_tid = thread.os_thread_id();

    // SAFETY: The caller guarantees the world is stopped and the thread's stack
    // is stable to read.
    unsafe {
      crate::stackwalk_fp::walk_gc_reloc_pairs_from_safepoint_context(
        &ctx,
        stack_bounds,
        stackmaps,
        |pair| {
          f(pair);
        },
      )
      .map_err(|source| crate::scan::ScanError::StackWalkFailed {
        thread_id,
        os_tid,
        fp: ctx.fp,
        ip: ctx.ip,
        source,
      })?;
    }
    Ok(())
  })?;

  Ok(())
}

/// Enumerate all GC root slots while the world is stopped.
///
/// Root sources (in order):
/// 1) Per-thread root scopes (runtime-native handle stack).
/// 2) Per-thread shadow stacks (runtime-native Rust handle scopes).
/// 3) Global/persistent roots registered via `rt_gc_register_root_slot` / `rt_gc_pin`.
/// 4) Persistent roots stored in the global handle table (`roots::PersistentHandleTable`).
/// 5) Stack roots described by LLVM statepoint stackmaps for each thread that is either:
///    - has observed `stop_epoch` (published `safepoint_epoch_observed == stop_epoch`), or
///    - is in a GC-safe ("NativeSafe") region with a published safepoint context.
///
/// # Panics
/// Panics if `stop_epoch` is not an odd (stop-the-world) epoch.
pub fn for_each_root_slot_world_stopped(
  stop_epoch: u64,
  f: impl FnMut(*mut *mut u8),
) -> Result<(), crate::scan::ScanError> {
  for_each_root_slot_world_stopped_with_stackmaps(stop_epoch, stackmaps_for_self(), f)
}

/// Like [`for_each_root_slot_world_stopped`], but uses the provided `stackmaps` blob instead of
/// loading `.llvm_stackmaps` from the current binary.
///
/// This exists to keep tests deterministic: integration tests are built as standalone binaries and
/// do not necessarily contain stackmap metadata.
#[doc(hidden)]
pub fn for_each_root_slot_world_stopped_with_stackmaps(
  stop_epoch: u64,
  stackmaps: Option<&crate::StackMaps>,
  mut f: impl FnMut(*mut *mut u8),
) -> Result<(), crate::scan::ScanError> {
  assert_eq!(
    stop_epoch & 1,
    1,
    "for_each_root_slot_world_stopped called with non-stop epoch {stop_epoch}"
  );

  // 1) Thread-local handle stacks.
  registry::for_each_thread(|thread| thread.for_each_handle_slot(|slot| f(slot)));

  // 2) Thread-local shadow stacks.
  registry::for_each_thread(|thread| {
    let mut stack = thread.shadow_stack();
    stack.for_each_root_slot(&mut f);
  });

  // 3) Global roots.
  crate::roots::global_root_registry().for_each_root_slot(|slot| f(slot));

  // 4) Persistent handle-table roots.
  crate::roots::global_persistent_handle_table().for_each_root_slot(|slot| f(slot));

  // 5) Stack roots from stackmaps.
  let Some(stackmaps) = stackmaps else {
    return Ok(());
  };

  let coordinator_id = registry::current_thread_id();
  registry::try_for_each_thread(|thread| -> Result<(), crate::scan::ScanError> {
    if thread.is_detached() {
      return Ok(());
    }
    if thread.is_parked() {
      return Ok(());
    }

    let is_coordinator = Some(thread.id()) == coordinator_id;
    if is_coordinator && thread.is_native_safe() {
      return Ok(());
    }
    if !thread.is_native_safe() && thread.safepoint_epoch_observed() != stop_epoch {
      return Ok(());
    }

    // The coordinator thread is not stopped inside `rt_gc_safepoint` (it is executing the GC),
    // so it may not have published a safepoint context. Skip it in that case.
    if is_coordinator && thread.safepoint_context().is_none() {
      return Ok(());
    }

    crate::scan::scan_thread_roots(thread.as_ref(), stackmaps, &mut f)?;
    Ok(())
  })?;

  Ok(())
}

/// Enumerate all GC root relocation pairs while the world is stopped.
///
/// Root sources (in order):
/// 1) Per-thread root scopes (runtime-native handle stack).
/// 2) Per-thread shadow stacks (runtime-native Rust handle scopes).
/// 3) Global/persistent roots registered via `rt_gc_register_root_slot` / `rt_gc_pin`.
/// 4) Persistent roots stored in the global handle table (`roots::PersistentHandleTable`).
/// 5) Stack roots described by LLVM statepoint stackmaps for each thread that is either:
///    - has observed `stop_epoch` (published `safepoint_epoch_observed == stop_epoch`), or
///    - is in a GC-safe ("NativeSafe") region with a published safepoint context.
///
/// For non-stack roots, the pair is `(base_slot == derived_slot)`.
///
/// # Panics
/// Panics if `stop_epoch` is not an odd (stop-the-world) epoch.
pub fn for_each_root_reloc_pair_world_stopped(
  stop_epoch: u64,
  f: impl FnMut(RelocPair),
) -> Result<(), crate::scan::ScanError> {
  // Backwards-compatible alias: keep the original API name requested by older callers, but delegate
  // to the canonical reloc-pair enumerator.
  for_each_reloc_pair_world_stopped(stop_epoch, f)
}

#[cfg(test)]
mod tests {
  use crate::alloc;
  use crate::arch::SafepointContext;
  use crate::gc::ObjHeader;
  use crate::gc::TypeDescriptor;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::Arc;
  use std::sync::Barrier;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
 
  #[repr(C)]
  struct Obj {
    header: ObjHeader,
    value: usize,
  }
 
  static OBJ_DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<Obj>(), &[]);
 
  fn alloc_obj(value: usize) -> *mut u8 {
    let size = core::mem::size_of::<Obj>();
    let align = core::mem::align_of::<Obj>();
    let obj = alloc::alloc_bytes(size, align, "safepoint test");
    unsafe {
      core::ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = &OBJ_DESC as *const TypeDescriptor;
      header.meta = AtomicUsize::new(0);
      (*(obj as *mut Obj)).value = value;
    }
    obj
  }
  
  #[test]
  fn fixup_safepoint_context_recovers_nearest_managed_callsite() {
    let stackmaps = crate::StackMaps::parse(include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/tests/fixtures/bin/statepoint_x86_64.bin"
    )))
    .expect("parse stackmaps");
    let (managed_ra, _) = stackmaps.iter().next().expect("fixture should contain at least one callsite");

    // Build a synthetic runtime->managed frame chain in a stack-allocated buffer. `find_nearest_managed_cursor`
    // bounds-checks against the current thread's *real* stack, so this must live on the stack (not heap).
    let mut stack = [0u8; 512];
    let base = stack.as_mut_ptr() as usize;

    // Inner/runtime frame (lower address), outer/managed frame (higher address).
    let runtime_fp = align_up(base + 64, 16);
    let managed_fp = align_up(base + 256, 16);
    assert!(managed_fp > runtime_fp);

    unsafe {
      // Runtime frame record:
      //   [FP + 0] = saved FP
      //   [FP + 8] = saved return address into caller
      write_u64(runtime_fp + 0, managed_fp as u64);
      write_u64(runtime_fp + 8, managed_ra);

      // Managed frame record: terminate the chain.
      write_u64(managed_fp + 0, 0);
      write_u64(managed_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: runtime_fp,
      ip: 0xdead_beef_dead_beefusize,
      ..Default::default()
    };
    let fixed = super::fixup_safepoint_context_to_nearest_managed(ctx, Some(&stackmaps));

    assert_eq!(fixed.fp, managed_fp);
    assert_eq!(fixed.ip as u64, managed_ra);
    assert!(stackmaps.lookup(fixed.ip as u64).is_some());

    let expected_sp_callsite = runtime_fp as u64 + 16;
    #[cfg(target_arch = "x86_64")]
    let expected_sp_entry = expected_sp_callsite - crate::arch::WORD_SIZE as u64;
    #[cfg(not(target_arch = "x86_64"))]
    let expected_sp_entry = expected_sp_callsite;

    assert_eq!(fixed.sp as u64, expected_sp_callsite);
    assert_eq!(fixed.sp_entry as u64, expected_sp_entry);
  }

  #[test]
  fn fixup_safepoint_context_uses_override_start_fp_when_fp_chain_is_broken() {
    let stackmaps = crate::StackMaps::parse(include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/tests/fixtures/bin/statepoint_x86_64.bin"
    )))
    .expect("parse stackmaps");
    let (managed_ra, _) = stackmaps.iter().next().expect("fixture should contain at least one callsite");

    let mut stack = [0u8; 512];
    let base = stack.as_mut_ptr() as usize;

    // Simulate a `catch_unwind` frame that does not maintain an FP chain by making the
    // "captured" FP's saved caller pointer bogus. Provide a separate override FP
    // that links to a stackmapped return address.
    let bad_fp = align_up(base + 64, 16);
    let runtime_fp = align_up(base + 160, 16);
    let managed_fp = align_up(base + 320, 16);

    assert!(runtime_fp > bad_fp);
    assert!(managed_fp > runtime_fp);

    unsafe {
      // Broken frame record (fp_caller == 0).
      write_u64(bad_fp + 0, 0);
      write_u64(bad_fp + 8, 0x1111_2222_3333_4444);

      // Outer runtime frame that links to a managed callsite.
      write_u64(runtime_fp + 0, managed_fp as u64);
      write_u64(runtime_fp + 8, managed_ra);

      // Managed frame record: terminate the chain.
      write_u64(managed_fp + 0, 0);
      write_u64(managed_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: bad_fp,
      ip: 0xdead_beef_dead_beefusize,
      ..Default::default()
    };

    let fixed = super::with_safepoint_fixup_start_fp(runtime_fp as u64, || {
      super::fixup_safepoint_context_to_nearest_managed(ctx, Some(&stackmaps))
    });

    assert_eq!(fixed.fp, managed_fp);
    assert_eq!(fixed.ip as u64, managed_ra);
    assert!(stackmaps.lookup(fixed.ip as u64).is_some());

    let expected_sp_callsite = runtime_fp as u64 + 16;
    #[cfg(target_arch = "x86_64")]
    let expected_sp_entry = expected_sp_callsite - crate::arch::WORD_SIZE as u64;
    #[cfg(not(target_arch = "x86_64"))]
    let expected_sp_entry = expected_sp_callsite;

    assert_eq!(fixed.sp as u64, expected_sp_callsite);
    assert_eq!(fixed.sp_entry as u64, expected_sp_entry);
  }

  #[test]
  fn stw_safepoint_barrier_is_deadlock_free() {
    const WORKERS: usize = 4;
    const WORKER_ITERS: usize = 2_000;
    const GC_ITERS: usize = 20;
    let _rt = crate::test_util::TestRuntimeGuard::new();
  
    // Register the coordinator (main test thread) so it participates in STW accounting.
    threading::register_current_thread(ThreadKind::Main);
 
    let barrier = Arc::new(Barrier::new(WORKERS + 1));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(WORKERS);
 
    for idx in 0..WORKERS {
      let barrier = barrier.clone();
      let completed = completed.clone();
      handles.push(std::thread::spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
 
        // Root a single object through the per-thread handle stack.
        let mut root: *mut u8 = core::ptr::null_mut();
        let mut scope = crate::roots::RootScope::new();
        scope.push(&mut root as *mut *mut u8);
        root = alloc_obj(idx);
 
        barrier.wait();
 
        for _ in 0..WORKER_ITERS {
          crate::rt_gc_safepoint();
          // Allocate a little garbage to keep the mutator doing work between safepoints.
          let _ = alloc_obj(idx.wrapping_add(1000));
        }
 
        // Ensure the rooted object remains readable after repeated STW pauses.
        unsafe {
          assert_eq!((*(root as *mut Obj)).value, idx);
        }
        completed.fetch_add(1, Ordering::Release);
        // Unregistering the thread may contend on GC-aware locks, which can temporarily transition
        // into a GC-safe region. Ensure the per-thread handle stack is empty before that happens:
        // GC-safe regions must not have live raw GC pointers in local stack slots.
        drop(scope);
        threading::unregister_current_thread();
      }));
    }
 
    // Let all workers start their loops.
    barrier.wait();
 
    for _ in 0..GC_ITERS {
      crate::rt_gc_collect();
    }
 
    for h in handles {
      h.join().unwrap();
    }
 
    assert_eq!(completed.load(Ordering::Acquire), WORKERS);
    threading::unregister_current_thread();
  }

  fn align_up(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
  }

  unsafe fn write_u64(addr: usize, val: u64) {
    (addr as *mut u64).write_unaligned(val);
  }
}
