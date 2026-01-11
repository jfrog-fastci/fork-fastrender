use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::cell::Cell;

pub(crate) type MicrotaskCheckpointEndHook = Box<dyn FnMut() + Send + 'static>;

static MICROTASK_CHECKPOINT_END_HOOK: Lazy<Mutex<Option<MicrotaskCheckpointEndHook>>> =
  Lazy::new(|| Mutex::new(None));

thread_local! {
  static PERFORMING_MICROTASK_CHECKPOINT: Cell<bool> = const { Cell::new(false) };
}

struct MicrotaskCheckpointGuard;

impl MicrotaskCheckpointGuard {
  fn enter() -> Option<Self> {
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
