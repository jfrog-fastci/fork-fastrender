use crate::abi::PromiseRef;
use crate::abi::PromiseResolveInput;
use crate::abi::PromiseResolveKind;
use crate::abi::RtCoroStatus;
use crate::abi::RtCoroutineHeader;

use super::promise::{
  promise_mark_handled, promise_new, promise_outcome, promise_register_reaction, promise_resolve_into, PromiseOutcome,
};
use super::strict_await_yields;
use super::{queue_macrotask, queue_microtask, TaskFn};

extern "C" fn coro_resume_task(data: *mut u8) {
  let coro = data as *mut RtCoroutineHeader;
  // Safety: the caller is responsible for keeping the coroutine frame alive until completion.
  run_coroutine(coro);
}

fn schedule_resume_macrotask(coro: *mut RtCoroutineHeader) {
  queue_macrotask(coro_resume_task as TaskFn, coro as *mut u8);
}

fn schedule_resume_microtask(coro: *mut RtCoroutineHeader) {
  queue_microtask(coro_resume_task as TaskFn, coro as *mut u8);
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

#[repr(C)]
struct AwaitReaction {
  node: crate::promise_reactions::PromiseReactionNode,
  coro: *mut RtCoroutineHeader,
}

extern "C" fn await_reaction_run(
  node: *mut crate::promise_reactions::PromiseReactionNode,
  promise: crate::async_abi::PromiseRef,
) {
  let node = unsafe { &*(node as *mut AwaitReaction) };
  let awaited = PromiseRef(promise.cast());

  let (await_is_error, await_value, await_error) = match promise_outcome(awaited) {
    PromiseOutcome::Fulfilled(v) => (0, v, core::ptr::null_mut()),
    PromiseOutcome::Rejected(e) => (1, core::ptr::null_mut(), e),
    PromiseOutcome::Pending => {
      // Should not happen (reactions are scheduled after settlement), but be robust: resubscribe.
      let new_node = alloc_await_reaction(node.coro);
      promise_register_reaction(awaited, new_node);
      return;
    }
  };

  if !node.coro.is_null() {
    unsafe {
      (*node.coro).await_is_error = await_is_error;
      (*node.coro).await_value = await_value;
      (*node.coro).await_error = await_error;
    }
    run_coroutine(node.coro);
  }
}

extern "C" fn await_reaction_drop(node: *mut crate::promise_reactions::PromiseReactionNode) {
  unsafe {
    drop(Box::from_raw(node as *mut AwaitReaction));
  }
}

static AWAIT_REACTION_VTABLE: crate::promise_reactions::PromiseReactionVTable =
  crate::promise_reactions::PromiseReactionVTable {
    run: await_reaction_run,
    drop: await_reaction_drop,
  };

fn alloc_await_reaction(coro: *mut RtCoroutineHeader) -> *mut crate::promise_reactions::PromiseReactionNode {
  let node = Box::new(AwaitReaction {
    node: crate::promise_reactions::PromiseReactionNode {
      next: core::ptr::null_mut(),
      vtable: &AWAIT_REACTION_VTABLE,
    },
    coro,
  });
  Box::into_raw(node) as *mut crate::promise_reactions::PromiseReactionNode
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

pub(crate) fn async_spawn_deferred(coro: *mut RtCoroutineHeader) -> PromiseRef {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return PromiseRef::null();
  }

  unsafe {
    if (*coro).promise.is_null() {
      (*coro).promise = promise_new();
    }
  }

  schedule_resume_microtask(coro);

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

  // `await` always attaches a rejection handler (even if it only propagates the error), so it
  // counts as handling the awaited promise for unhandled-rejection tracking. This must happen even
  // when we take the settled fast-path (synchronous resumption), because attaching handlers after
  // rejection should trigger `rejectionhandled` behavior.
  promise_mark_handled(awaited);

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

  let node = alloc_await_reaction(coro);
  promise_register_reaction(awaited, node);
}

pub(crate) fn coro_await_value(coro: *mut RtCoroutineHeader, awaited: PromiseResolveInput, next_state: u32) {
  match awaited.kind {
    PromiseResolveKind::Promise => {
      let p = unsafe { awaited.payload.promise };
      coro_await(coro, p, next_state);
    }
    PromiseResolveKind::Value | PromiseResolveKind::Thenable => {
      // Await semantics are equivalent to `PromiseResolve` + `then`.
      let p = promise_new();
      promise_resolve_into(p, awaited);
      coro_await(coro, p, next_state);
    }
  }
}
