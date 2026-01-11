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
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};

use crate::async_abi::{
  Coroutine, CoroutineRef, CoroutineStepTag, CORO_FLAG_DESTROYED, CORO_FLAG_RUNTIME_OWNS_FRAME,
};

// -----------------------------------------------------------------------------
// Microtask checkpoint helpers
// -----------------------------------------------------------------------------

pub(crate) type MicrotaskCheckpointEndHook = Box<dyn FnMut() + Send + 'static>;

static MICROTASK_CHECKPOINT_END_HOOK: Lazy<Mutex<Option<MicrotaskCheckpointEndHook>>> =
  Lazy::new(|| Mutex::new(None));

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

pub(crate) fn reset_for_tests() {
  PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.set(false));
  *MICROTASK_CHECKPOINT_END_HOOK.lock() = None;
}

pub(crate) fn set_microtask_checkpoint_end_hook(hook: Option<MicrotaskCheckpointEndHook>) {
  *MICROTASK_CHECKPOINT_END_HOOK.lock() = hook;
}

fn run_microtask_checkpoint_end_hook() {
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

pub fn rt_drain_microtasks() -> bool {
  let Some(_guard) = MicrotaskCheckpointGuard::enter() else {
    return false;
  };

  let did_work = crate::async_rt::drain_microtasks_nonblocking();
  crate::unhandled_rejection::microtask_checkpoint();
  run_microtask_checkpoint_end_hook();
  did_work
}

pub fn rt_async_run_until_idle() -> bool {
  let Some(_guard) = MicrotaskCheckpointGuard::enter() else {
    return false;
  };

  let did_work = crate::async_rt::run_until_idle_nonblocking();
  crate::unhandled_rejection::microtask_checkpoint();
  run_microtask_checkpoint_end_hook();
  did_work
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
  queued: VecDeque<usize>,

  /// Set of coroutine frame addresses owned by the runtime and not yet destroyed.
  ///
  /// This guards against double-destroy and allows resume/cancel paths to avoid
  /// dereferencing freed frames: if a coroutine address is absent from this set,
  /// it must be treated as dead.
  live_owned: HashSet<usize>,
}

static CORO_STATE: Lazy<Mutex<AsyncCoroState>> = Lazy::new(|| Mutex::new(AsyncCoroState {
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
pub(crate) unsafe fn coro_is_runtime_owned(coro: CoroutineRef) -> bool {
  (*coro).flags & CORO_FLAG_RUNTIME_OWNS_FRAME != 0
}

#[inline]
pub(crate) fn coro_is_live_owned(coro: CoroutineRef) -> bool {
  CORO_STATE.lock().live_owned.contains(&(coro as usize))
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

pub(crate) unsafe fn coro_destroy_once(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  if !coro_is_runtime_owned(coro) {
    // Stack/caller-owned frame: the runtime must never destroy it.
    return;
  }

  // Remove from the live-set first. This ensures:
  // - double-destroys are suppressed,
  // - and other queued resume/cancel paths can check liveness without dereferencing `coro`.
  let removed = CORO_STATE.lock().live_owned.remove(&(coro as usize));
  if !removed {
    return;
  }

  coro_destroy_now(coro);
}

/// Register a coroutine frame with the runtime's ownership tracker.
///
/// If `CORO_FLAG_RUNTIME_OWNS_FRAME` is set in `coro.flags`, the coroutine is recorded in the
/// internal live set so that:
/// - it can be destroyed exactly once on completion/cancellation, and
/// - stale scheduled resumes can check liveness without dereferencing a freed frame.
///
/// # Safety
/// `coro` must point to a valid [`Coroutine`] header.
pub(crate) unsafe fn track_coro_if_runtime_owned(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }
  if coro_is_runtime_owned(coro) {
    CORO_STATE.lock().live_owned.insert(coro as usize);
  }
}

fn enqueue_awaiting(coro: CoroutineRef) {
  CORO_STATE.lock().queued.push_back(coro as usize);
}

unsafe fn run_coroutine(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  let vtable = (*coro).vtable;
  if vtable.is_null() {
    std::process::abort();
  }

  let step = ((*vtable).resume)(coro);
  match step.tag {
    CoroutineStepTag::Complete => {
      coro_destroy_once(coro);
    }
    CoroutineStepTag::Await => {
      // A coroutine that yields must be stored across turns. Stack-owned frames cannot be
      // referenced after the spawning call returns.
      if cfg!(debug_assertions) && !coro_is_runtime_owned(coro) {
        eprintln!(
          "runtime-native async ABI violation: coroutine yielded `Await` but \
CORO_FLAG_RUNTIME_OWNS_FRAME was not set (stack-owned coroutine frames must not suspend)"
        );
        std::process::abort();
      }
      enqueue_awaiting(coro);
    }
  }
}

/// Spawn a coroutine and run it synchronously until it either yields (`Await`) or completes.
///
/// This is a *Rust-level* helper for driving the `async_abi` coroutine layout. The exported C ABI
/// surface is owned elsewhere.
///
/// # Safety
/// `coro` must point to a valid coroutine frame with a valid `vtable`.
pub unsafe fn rt_async_spawn(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  if coro_is_runtime_owned(coro) {
    CORO_STATE.lock().live_owned.insert(coro as usize);
  }

  run_coroutine(coro);
}

/// Resume a coroutine previously spawned into the runtime.
///
/// This function is robust against stale scheduling: if the coroutine frame has already been
/// destroyed (by completion or cancellation), this is a no-op.
///
/// # Safety
/// `coro` must be a `CoroutineRef` previously passed to [`rt_async_spawn`].
pub unsafe fn rt_async_resume(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  // Only runtime-owned coroutines can be resumed asynchronously; stack-owned coroutines must not
  // yield and therefore never need to be resumed.
  if coro_is_runtime_owned(coro) && !coro_is_live_owned(coro) {
    return;
  }

  run_coroutine(coro);
}

/// Cancel (destroy) all runtime-owned coroutine frames that are currently queued/owned by this
/// runtime.
///
/// This is intended for teardown paths; it is exposed as a C ABI entrypoint via
/// [`crate::rt_async_cancel_all`].
pub fn cancel_all() {
  let coros: Vec<CoroutineRef> = {
    let mut state = CORO_STATE.lock();
    state.queued.clear();
    state
      .live_owned
      .drain()
      .map(|addr| addr as CoroutineRef)
      .collect()
  };

  for coro in coros {
    // Safety: `live_owned` only contains runtime-owned coroutines that have not been destroyed yet.
    unsafe {
      coro_destroy_now(coro);
    }
  }
}

/// Test helper: clear all internal coroutine state and destroy any live runtime-owned coroutine frames.
pub(crate) fn clear_state_for_tests() {
  cancel_all();
}
