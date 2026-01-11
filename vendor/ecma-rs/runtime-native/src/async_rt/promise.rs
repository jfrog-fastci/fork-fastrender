use crate::abi::PromiseRef;
use crate::abi::ValueRef;

use super::queue_microtask;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromiseState {
  Pending,
  Fulfilled,
  Rejected,
}

#[derive(Clone, Copy)]
struct PromiseContinuation {
  callback: extern "C" fn(*mut u8),
  data: *mut u8,
}

struct PromiseInner {
  state: PromiseState,
  value: ValueRef,
  error: ValueRef,
  continuations: Vec<PromiseContinuation>,
}

/// Minimal promise implementation sufficient for async/await lowering.
///
/// This type is opaque over the C ABI (`PromiseRef` is an opaque pointer handle).
pub struct RtPromise {
  inner: std::sync::Mutex<PromiseInner>,
}

impl RtPromise {
  fn new_pending() -> Self {
    Self {
      inner: std::sync::Mutex::new(PromiseInner {
        state: PromiseState::Pending,
        value: core::ptr::null_mut(),
        error: core::ptr::null_mut(),
        continuations: Vec::new(),
      }),
    }
  }
}

pub(crate) enum PromiseOutcome {
  Pending,
  Fulfilled(ValueRef),
  Rejected(ValueRef),
}

pub(crate) fn promise_outcome(p: PromiseRef) -> PromiseOutcome {
  if p.is_null() {
    return PromiseOutcome::Pending;
  }
  let guard = unsafe { &*(p.0 as *mut RtPromise) }
    .inner
    .lock()
    .expect("promise mutex poisoned");
  match guard.state {
    PromiseState::Pending => PromiseOutcome::Pending,
    PromiseState::Fulfilled => PromiseOutcome::Fulfilled(guard.value),
    PromiseState::Rejected => PromiseOutcome::Rejected(guard.error),
  }
}

pub(crate) fn promise_new() -> PromiseRef {
  PromiseRef(Box::into_raw(Box::new(RtPromise::new_pending())) as *mut core::ffi::c_void)
}

pub(crate) fn promise_then(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  if p.is_null() {
    // Treat null as "never settles": keep it pending.
    return;
  }

  let mut schedule_now = false;
  {
    let mut guard = unsafe { &*(p.0 as *mut RtPromise) }
      .inner
      .lock()
      .expect("promise mutex poisoned");
    match guard.state {
      PromiseState::Pending => {
        guard.continuations.push(PromiseContinuation {
          callback: on_settle,
          data,
        });
      }
      PromiseState::Fulfilled | PromiseState::Rejected => {
        schedule_now = true;
      }
    }
  }

  if schedule_now {
    queue_microtask(on_settle, data);
  }
}

pub(crate) fn promise_resolve(p: PromiseRef, value: ValueRef) {
  if p.is_null() {
    return;
  }

  let continuations = {
    let mut guard = unsafe { &*(p.0 as *mut RtPromise) }
      .inner
      .lock()
      .expect("promise mutex poisoned");
    if guard.state != PromiseState::Pending {
      return;
    }
    guard.state = PromiseState::Fulfilled;
    guard.value = value;
    guard.error = core::ptr::null_mut();
    core::mem::take(&mut guard.continuations)
  };

  for cont in continuations {
    queue_microtask(cont.callback, cont.data);
  }
}

pub(crate) fn promise_reject(p: PromiseRef, err: ValueRef) {
  if p.is_null() {
    return;
  }

  let continuations = {
    let mut guard = unsafe { &*(p.0 as *mut RtPromise) }
      .inner
      .lock()
      .expect("promise mutex poisoned");
    if guard.state != PromiseState::Pending {
      return;
    }
    guard.state = PromiseState::Rejected;
    guard.error = err;
    guard.value = core::ptr::null_mut();
    core::mem::take(&mut guard.continuations)
  };

  for cont in continuations {
    queue_microtask(cont.callback, cont.data);
  }
}
