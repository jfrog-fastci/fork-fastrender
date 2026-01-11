//! Async-runtime helpers for `runtime-native`.
//!
//! This module currently contains three largely-orthogonal pieces:
//!
//! 1. **Microtask checkpoint helpers** (`rt_drain_microtasks`, `rt_async_run_until_idle`) with
//!    non-reentrancy enforcement (HTML-style microtask checkpoint semantics).
//! 2. **Promise payload layout helpers** ([`PromiseLayout`]) for parallel task spawning.
//! 3. **Coroutine frame ownership/lifetime tracking** for the `async_abi` coroutine layout
//!    (heap-owned vs stack-owned frames, and destroy-once semantics).

use once_cell::sync::Lazy;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::async_abi::{
  Coroutine, CoroutineRef, CORO_FLAG_DESTROYED, CORO_FLAG_RUNTIME_OWNS_FRAME,
};
use crate::CoroutineId;
use crate::sync::GcAwareMutex;

// -----------------------------------------------------------------------------
// Microtask checkpoint helpers
// -----------------------------------------------------------------------------

pub(crate) type MicrotaskCheckpointEndHook = Box<dyn FnMut() + Send + 'static>;

static MICROTASK_CHECKPOINT_END_HOOK: Lazy<GcAwareMutex<Option<MicrotaskCheckpointEndHook>>> =
  Lazy::new(|| GcAwareMutex::new(None));

thread_local! {
  static PERFORMING_MICROTASK_CHECKPOINT: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct MicrotaskCheckpointGuard;

impl MicrotaskCheckpointGuard {
  pub(crate) fn enter() -> Option<Self> {
    let already_in_checkpoint =
      PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.replace(true));
    if already_in_checkpoint {
      return None;
    }
    Some(Self)
  }
}

impl Drop for MicrotaskCheckpointGuard {
  fn drop(&mut self) {
    PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.set(false));
  }
}

const DEFAULT_MAX_READY_STEPS_PER_POLL: usize = 100_000;
const DEFAULT_MAX_READY_QUEUE_LEN: usize = 100_000;

static MAX_READY_STEPS_PER_POLL: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_READY_STEPS_PER_POLL);
static MAX_READY_QUEUE_LEN: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_READY_QUEUE_LEN);

static LAST_ERROR: Lazy<GcAwareMutex<Option<String>>> = Lazy::new(|| GcAwareMutex::new(None));

pub(crate) fn reset_for_tests() {
  PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.set(false));
  *MICROTASK_CHECKPOINT_END_HOOK.lock() = None;
  *LAST_ERROR.lock() = None;
  MAX_READY_STEPS_PER_POLL.store(DEFAULT_MAX_READY_STEPS_PER_POLL, Ordering::Release);
  MAX_READY_QUEUE_LEN.store(DEFAULT_MAX_READY_QUEUE_LEN, Ordering::Release);
}

/// Reset internal bookkeeping so the async runtime can continue after teardown/cancellation.
///
/// Unlike [`reset_for_tests`], this preserves user-configured limits and hooks.
pub(crate) fn reset_after_cancel() {
  PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.set(false));
  *LAST_ERROR.lock() = None;
}

pub(crate) fn set_microtask_checkpoint_end_hook(hook: Option<MicrotaskCheckpointEndHook>) {
  *MICROTASK_CHECKPOINT_END_HOOK.lock() = hook;
}

fn run_microtask_checkpoint_end_hook() {
  // Process unhandled promise rejections at the end of a microtask checkpoint (HTML-shaped).
  crate::promise_api::microtask_checkpoint_end();

  struct HookRestore(Option<MicrotaskCheckpointEndHook>);

  impl Drop for HookRestore {
    fn drop(&mut self) {
      *MICROTASK_CHECKPOINT_END_HOOK.lock() = self.0.take();
    }
  }

  let hook = MICROTASK_CHECKPOINT_END_HOOK.lock().take();
  let mut restore = HookRestore(hook);
  if let Some(hook) = restore.0.as_mut() {
    hook();
  }
}

fn run_microtask_checkpoint(drive: impl FnOnce() -> bool) -> bool {
  let Some(_guard) = MicrotaskCheckpointGuard::enter() else {
    return false;
  };

  if has_error() {
    crate::unhandled_rejection::microtask_checkpoint();
    run_microtask_checkpoint_end_hook();
    return false;
  }

  let did_work = drive();
  crate::unhandled_rejection::microtask_checkpoint();
  run_microtask_checkpoint_end_hook();
  if has_error() {
    false
  } else {
    did_work
  }
}

pub fn rt_drain_microtasks() -> bool {
  run_microtask_checkpoint(|| crate::async_rt::drain_microtasks_nonblocking())
}

pub fn rt_async_run_until_idle() -> bool {
  // If the executor has entered an error state (runaway detection), it will no longer make forward
  // progress. Avoid spinning here (and aborting via the internal runaway turn limit) and instead
  // return so callers can retrieve the error via `rt_async_take_last_error`.
  run_microtask_checkpoint(|| crate::async_rt::run_until_idle_nonblocking())
}

pub(crate) fn rt_async_run_until_idle_under_driver_guard() -> bool {
  run_microtask_checkpoint(|| crate::async_rt::run_until_idle_nonblocking_under_driver_guard())
}
/// Layout of the payload storage associated with a promise returned by
/// `rt_parallel_spawn_promise`.
///
/// The runtime uses this to allocate a payload buffer; the parallel task writes
/// its result into the buffer (via `rt_promise_payload_ptr`) and then settles the
/// promise (via `rt_promise_fulfill` / `rt_promise_reject_payload`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PromiseLayout {
  pub size: usize,
  pub align: usize,
}

impl PromiseLayout {
  #[inline]
  pub const fn of<T>() -> Self {
    Self {
      size: core::mem::size_of::<T>(),
      align: core::mem::align_of::<T>(),
    }
  }
}

// -----------------------------------------------------------------------------
// Coroutine frame ownership tracking (`async_abi`)
// -----------------------------------------------------------------------------

struct AsyncCoroState {
  /// Coroutines that have yielded `Await` and are stored across turns.
  ///
  /// Note: this is *not* a "ready queue"; it's used for cancellation/teardown.
  queued: VecDeque<CoroutineId>,

  /// Set of coroutine handles owned by the runtime and not yet destroyed.
  ///
  /// This is used for teardown (`rt_async_cancel_all`). We store stable `CoroutineId` handles (not
  /// raw pointers) because coroutine frames may be relocated by a moving GC and because queued
  /// coroutines may cross OS/thread boundaries.
  live_owned: HashSet<CoroutineId>,
}

static CORO_STATE: Lazy<GcAwareMutex<AsyncCoroState>> = Lazy::new(|| GcAwareMutex::new(AsyncCoroState {
  queued: VecDeque::new(),
  live_owned: HashSet::new(),
}));

#[inline]
fn validate_coro_ptr(coro: CoroutineRef) -> CoroutineRef {
  if coro.is_null() {
    return coro;
  }
  if (coro as usize) % core::mem::align_of::<Coroutine>() != 0 {
    std::process::abort();
  }
  coro
}

#[inline]
fn coro_load(id: CoroutineId) -> Option<CoroutineRef> {
  let ptr = crate::rt_handle_load(id.0);
  let coro = validate_coro_ptr(ptr.cast::<Coroutine>());
  (!coro.is_null()).then_some(coro)
}

unsafe fn coro_destroy_now(coro: CoroutineRef) {
  debug_assert!(!coro.is_null());

  let vtable = (*coro).vtable;
  if vtable.is_null() {
    std::process::abort();
  }

  // Mark as destroyed before calling out to user code.
  (*coro).flags |= CORO_FLAG_DESTROYED;

  // Safety: caller is responsible for ensuring `coro` is owned by the runtime.
  ((*vtable).destroy)(coro);
}

pub(crate) fn coro_destroy_once(id: CoroutineId) {
  // Remove from the live set first so cancellation paths never double-destroy and so teardown
  // (`rt_async_cancel_all`) doesn't try to destroy the same coroutine twice.
  CORO_STATE.lock().live_owned.remove(&id);

  // Load the current coroutine pointer from the persistent-handle table.
  let Some(coro) = coro_load(id) else {
    // Always free the handle (idempotent) so stale scheduled resumes become no-ops and so we don't
    // leak the handle table entry.
    crate::rt_handle_free(id.0);
    return;
  };

  let flags = unsafe { (*coro).flags };
  let runtime_owned = (flags & CORO_FLAG_RUNTIME_OWNS_FRAME) != 0;
  let already_destroyed = (flags & CORO_FLAG_DESTROYED) != 0;

  if runtime_owned && !already_destroyed {
    unsafe {
      coro_destroy_now(coro);
    }
  }

  // Free the stable handle after destroying the frame. This is idempotent and ensures any stale
  // scheduled resumes become no-ops.
  crate::rt_handle_free(id.0);
}

/// Register a coroutine frame with the runtime's ownership tracker.
///
/// If `CORO_FLAG_RUNTIME_OWNS_FRAME` is set in `coro.flags`, the coroutine is recorded in the
/// internal live set so that:
/// - it can be destroyed exactly once on completion/cancellation, and
/// - stale scheduled resumes can check liveness without dereferencing a freed frame.
///
pub(crate) fn track_coro_if_runtime_owned(id: CoroutineId) {
  let Some(coro) = coro_load(id) else {
    return;
  };
  let flags = unsafe { (*coro).flags };
  if (flags & CORO_FLAG_RUNTIME_OWNS_FRAME) != 0 && (flags & CORO_FLAG_DESTROYED) == 0 {
    CORO_STATE.lock().live_owned.insert(id);
  }
}

/// Cancel (destroy) all runtime-owned coroutine frames that are currently queued/owned by this
/// runtime.
///
/// This is intended for teardown paths; it is exposed as a C ABI entrypoint via
/// [`crate::rt_async_cancel_all`].
pub fn cancel_all() {
  let coros: Vec<CoroutineId> = {
    let mut state = CORO_STATE.lock();
    state.queued.clear();
    state.live_owned.drain().collect()
  };

  for id in coros {
    coro_destroy_once(id);
  }
}

/// Test helper: clear all internal coroutine state and destroy any live runtime-owned coroutine frames.
pub(crate) fn clear_state_for_tests() {
  cancel_all();
}

pub(crate) fn set_limits(max_steps: usize, max_queue_len: usize) {
  MAX_READY_STEPS_PER_POLL.store(max_steps.max(1), Ordering::Release);
  MAX_READY_QUEUE_LEN.store(max_queue_len, Ordering::Release);
}

pub(crate) fn max_ready_steps_per_poll() -> usize {
  MAX_READY_STEPS_PER_POLL.load(Ordering::Acquire)
}

pub(crate) fn max_ready_queue_len() -> Option<usize> {
  match MAX_READY_QUEUE_LEN.load(Ordering::Acquire) {
    0 => None,
    v => Some(v),
  }
}

pub(crate) fn has_error() -> bool {
  LAST_ERROR.lock().is_some()
}

pub(crate) fn set_error_once(msg: impl Into<String>) {
  let mut guard = LAST_ERROR.lock();
  if guard.is_none() {
    *guard = Some(msg.into());
  }
}

pub(crate) fn take_last_error() -> Option<String> {
  LAST_ERROR.lock().take()
}
