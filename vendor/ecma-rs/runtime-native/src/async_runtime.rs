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
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::async_abi::{
  Coroutine, CoroutineRef, CoroutineStepTag, CORO_FLAG_DESTROYED, CORO_FLAG_RUNTIME_OWNS_FRAME,
};
use crate::gc::HandleId;

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

const DEFAULT_MAX_READY_STEPS_PER_POLL: usize = 100_000;
const DEFAULT_MAX_READY_QUEUE_LEN: usize = 100_000;

static MAX_READY_STEPS_PER_POLL: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_READY_STEPS_PER_POLL);
static MAX_READY_QUEUE_LEN: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_READY_QUEUE_LEN);

static LAST_ERROR: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

pub(crate) fn reset_for_tests() {
  PERFORMING_MICROTASK_CHECKPOINT.with(|performing| performing.set(false));
  *MICROTASK_CHECKPOINT_END_HOOK.lock() = None;
  *LAST_ERROR.lock() = None;
  MAX_READY_STEPS_PER_POLL.store(DEFAULT_MAX_READY_STEPS_PER_POLL, Ordering::Release);
  MAX_READY_QUEUE_LEN.store(DEFAULT_MAX_READY_QUEUE_LEN, Ordering::Release);
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

  if has_error() {
    crate::unhandled_rejection::microtask_checkpoint();
    run_microtask_checkpoint_end_hook();
    return false;
  }

  let did_work = crate::async_rt::drain_microtasks_nonblocking();
  crate::unhandled_rejection::microtask_checkpoint();
  run_microtask_checkpoint_end_hook();
  if has_error() {
    false
  } else {
    did_work
  }
}

pub fn rt_async_run_until_idle() -> bool {
  let Some(_guard) = MicrotaskCheckpointGuard::enter() else {
    return false;
  };

  // If the executor has entered an error state (runaway detection), it will no longer make forward
  // progress. Avoid spinning here (and aborting via the internal runaway turn limit) and instead
  // return so callers can retrieve the error via `rt_async_take_last_error`.
  if has_error() {
    crate::unhandled_rejection::microtask_checkpoint();
    run_microtask_checkpoint_end_hook();
    return false;
  }

  let did_work = crate::async_rt::run_until_idle_nonblocking();
  crate::unhandled_rejection::microtask_checkpoint();
  run_microtask_checkpoint_end_hook();
  if has_error() {
    false
  } else {
    did_work
  }
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
  queued: VecDeque<HandleId>,

  /// Set of coroutine handle ids owned by the runtime and not yet destroyed.
  ///
  /// This guards against double-destroy and allows resume/cancel paths to avoid
  /// dereferencing freed frames: if a coroutine id is absent from this set, it must be treated as
  /// dead.
  live_owned: HashSet<HandleId>,
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
pub(crate) fn coro_is_live_owned(id: HandleId) -> bool {
  CORO_STATE.lock().live_owned.contains(&id)
}

#[inline]
pub(crate) fn coro_ptr_if_live(id: HandleId) -> Option<CoroutineRef> {
  let state = CORO_STATE.lock();
  state.live_owned.contains(&id).then_some(())?;
  crate::roots::global_persistent_handle_table()
    .get(id)
    .map(|p| validate_coro_ptr(p.cast()))
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

pub(crate) unsafe fn coro_destroy_once(id: HandleId) {
  // Remove from the live-set first. This ensures:
  // - double-destroys are suppressed, and
  // - other queued resume/cancel paths can check liveness without dereferencing a freed frame.
  let removed = CORO_STATE.lock().live_owned.remove(&id);
  if !removed {
    return;
  }

  let Some(coro) = crate::roots::global_persistent_handle_table().get(id).map(|p| p.cast()) else {
    return;
  };
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  coro_destroy_now(coro);
  let _ = crate::roots::global_persistent_handle_table().free(id);
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
pub(crate) unsafe fn track_coro_if_runtime_owned(coro: CoroutineRef) -> Option<HandleId> {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return None;
  }
  if coro_is_runtime_owned(coro) {
    let id = crate::roots::global_persistent_handle_table().alloc(coro.cast());
    CORO_STATE.lock().live_owned.insert(id);
    return Some(id);
  }
  None
}

fn enqueue_awaiting(id: HandleId) {
  CORO_STATE.lock().queued.push_back(id);
}

unsafe fn run_coroutine(coro: CoroutineRef, id: Option<HandleId>) {
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
      if let Some(id) = id {
        coro_destroy_once(id);
      }
    }
    CoroutineStepTag::Await => {
      // A coroutine that yields must be stored across turns. Stack-owned frames cannot be
      // referenced after the spawning call returns.
      if cfg!(debug_assertions) && id.is_none() {
        eprintln!(
          "runtime-native async ABI violation: coroutine yielded `Await` but \
CORO_FLAG_RUNTIME_OWNS_FRAME was not set (stack-owned coroutine frames must not suspend)"
        );
        std::process::abort();
      }
      if let Some(id) = id {
        enqueue_awaiting(id);
      }
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

  let id = track_coro_if_runtime_owned(coro);
  run_coroutine(coro, id);
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
  let id = coro_is_runtime_owned(coro).then(|| {
    CORO_STATE
      .lock()
      .live_owned
      .iter()
      .find_map(|&id| {
        let Some(ptr) = crate::roots::global_persistent_handle_table().get(id) else {
          return None;
        };
        (ptr.cast::<Coroutine>() == coro).then_some(id)
      })
  });
  let id = id.flatten();

  // Robust against stale scheduling: if the runtime no longer owns the coroutine, this is a no-op.
  if let Some(id) = id {
    if !coro_is_live_owned(id) {
      return;
    }
  }

  run_coroutine(coro, id);
}

/// Cancel (destroy) all runtime-owned coroutine frames that are currently queued/owned by this
/// runtime.
///
/// This is intended for teardown paths; it is exposed as a C ABI entrypoint via
/// [`crate::rt_async_cancel_all`].
pub fn cancel_all() {
  let ids: Vec<HandleId> = {
    let mut state = CORO_STATE.lock();
    state.queued.clear();
    state.live_owned.drain().collect()
  };

  for id in ids {
    // Safety: `live_owned` only contains runtime-owned coroutines that have not been destroyed yet.
    unsafe {
      if let Some(coro) = crate::roots::global_persistent_handle_table().get(id).map(|p| p.cast()) {
        coro_destroy_now(validate_coro_ptr(coro));
      }
    }
    let _ = crate::roots::global_persistent_handle_table().free(id);
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
