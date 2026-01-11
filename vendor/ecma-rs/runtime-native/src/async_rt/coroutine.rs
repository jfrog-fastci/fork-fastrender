use crate::abi::PromiseRef;
use crate::abi::RtCoroStatus;
use crate::abi::RtCoroutineHeader;

use super::promise::{promise_new, promise_outcome, promise_then, PromiseOutcome};
use super::strict_await_yields;
use super::{queue_macrotask, TaskFn};

extern "C" fn coro_resume_task(data: *mut u8) {
  let coro = data as *mut RtCoroutineHeader;
  // Safety: the caller is responsible for keeping the coroutine frame alive until completion.
  run_coroutine(coro);
}

fn schedule_resume_macrotask(coro: *mut RtCoroutineHeader) {
  queue_macrotask(coro_resume_task as TaskFn, coro as *mut u8);
}

#[inline]
fn validate_coro_ptr(coro: *mut RtCoroutineHeader) -> *mut RtCoroutineHeader {
  if coro.is_null() {
    return coro;
  }
  if (coro as usize) % core::mem::align_of::<RtCoroutineHeader>() != 0 {
    std::process::abort();
  }
  coro
}

fn run_coroutine(coro: *mut RtCoroutineHeader) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  // Drive the coroutine until it yields, awaits, or completes.
  loop {
    let status = unsafe { ((*coro).resume)(coro) };
    match status {
      RtCoroStatus::Done | RtCoroStatus::Pending => break,
      RtCoroStatus::Yield => {
        schedule_resume_macrotask(coro);
        break;
      }
    }
  }
}

struct AwaitContinuation {
  coro: *mut RtCoroutineHeader,
  awaited: PromiseRef,
}

extern "C" fn await_on_settle(data: *mut u8) {
  // Safety: allocated by `coro_await`.
  let cont = unsafe { Box::from_raw(data as *mut AwaitContinuation) };

  let (await_is_error, await_value, await_error) = match promise_outcome(cont.awaited) {
    PromiseOutcome::Fulfilled(v) => (0, v, core::ptr::null_mut()),
    PromiseOutcome::Rejected(e) => (1, core::ptr::null_mut(), e),
    PromiseOutcome::Pending => {
      // Shouldn't happen (callback only runs after settlement) but be robust: resubscribe.
      promise_then(cont.awaited, await_on_settle, Box::into_raw(cont) as *mut u8);
      return;
    }
  };

  if !cont.coro.is_null() {
    unsafe {
      (*cont.coro).await_is_error = await_is_error;
      (*cont.coro).await_value = await_value;
      (*cont.coro).await_error = await_error;
    }
    run_coroutine(cont.coro);
  }
}

pub(crate) fn async_spawn(coro: *mut RtCoroutineHeader) -> PromiseRef {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return PromiseRef::null();
  }

  unsafe {
    if (*coro).promise.is_null() {
      (*coro).promise = promise_new();
    }
  }

  run_coroutine(coro);

  unsafe { (*coro).promise }
}

pub(crate) fn coro_await(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  unsafe {
    (*coro).state = next_state;
    (*coro).await_is_error = 0;
    (*coro).await_value = core::ptr::null_mut();
    (*coro).await_error = core::ptr::null_mut();
  }

  // Null promises are treated as "never settles" sentinels. Don't allocate a waiter or try to
  // register with the promise in that case.
  if awaited.is_null() {
    return;
  }

  // Fast-path: if the promise is already settled, resume the coroutine synchronously (unless strict
  // mode is requested).
  if !strict_await_yields() {
    match promise_outcome(awaited) {
      PromiseOutcome::Pending => {}
      PromiseOutcome::Fulfilled(v) => {
        unsafe {
          (*coro).await_is_error = 0;
          (*coro).await_value = v;
          (*coro).await_error = core::ptr::null_mut();
        }
        run_coroutine(coro);
        return;
      }
      PromiseOutcome::Rejected(e) => {
        unsafe {
          (*coro).await_is_error = 1;
          (*coro).await_value = core::ptr::null_mut();
          (*coro).await_error = e;
        }
        run_coroutine(coro);
        return;
      }
    }
  }

  let cont = Box::new(AwaitContinuation { coro, awaited });
  promise_then(awaited, await_on_settle, Box::into_raw(cont) as *mut u8);
}
