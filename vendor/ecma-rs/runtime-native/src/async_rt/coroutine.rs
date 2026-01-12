use crate::abi::{LegacyPromiseRef, PromiseRef};
use crate::abi::PromiseResolveInput;
use crate::abi::PromiseResolveKind;
use crate::abi::RtCoroStatus;
use crate::abi::RtCoroutineHeader;
use crate::gc::OBJ_HEADER_SIZE;

use super::promise::{
  promise_mark_handled, promise_new, promise_outcome, promise_register_reaction, promise_resolve_into, PromiseOutcome,
};
use super::strict_await_yields;
use super::{gc, global as async_global, Task, TaskFn};

#[inline]
fn promise_ref_from_legacy(p: LegacyPromiseRef) -> PromiseRef {
  PromiseRef(p.cast())
}

#[inline]
fn legacy_from_promise_ref(p: PromiseRef) -> LegacyPromiseRef {
  p.0.cast()
}

#[inline]
fn coro_from_obj_ptr(obj: *mut u8) -> *mut RtCoroutineHeader {
  if obj.is_null() {
    return core::ptr::null_mut();
  }
  // Safety: `obj` is expected to be a GC object base pointer (ObjHeader). The coroutine frame
  // header is stored in the payload immediately after the ObjHeader prefix.
  unsafe { obj.add(OBJ_HEADER_SIZE).cast::<RtCoroutineHeader>() }
}

#[inline]
fn coro_obj_ptr(coro: *mut RtCoroutineHeader) -> *mut u8 {
  if coro.is_null() {
    return core::ptr::null_mut();
  }
  // Safety: coroutine pointers are derived pointers into a GC-managed allocation:
  //
  //   obj_base == (coro as *mut u8) - OBJ_HEADER_SIZE
  //
  // The GC only understands object base pointers (ObjHeader), so any persistent rooting must use
  // the base pointer and re-derive the coroutine pointer at the use site.
  unsafe { (coro as *mut u8).sub(OBJ_HEADER_SIZE) }
}

extern "C" fn coro_resume_task(data: *mut u8) {
  let coro = coro_from_obj_ptr(data);
  run_coroutine(coro);
}

fn schedule_resume_macrotask(coro: *mut RtCoroutineHeader) {
  let obj = coro_obj_ptr(coro);
  // Safety: `obj` is the base pointer of a GC-managed coroutine frame. The task must keep it alive
  // until it runs, and `Task::run` ensures the callback observes the updated (relocated) pointer.
  unsafe {
    async_global().enqueue_macrotask(Task::new_gc_rooted(coro_resume_task as TaskFn, obj));
  }
}

fn schedule_resume_microtask(coro: *mut RtCoroutineHeader) {
  let obj = coro_obj_ptr(coro);
  // Safety: `obj` is the base pointer of a GC-managed coroutine frame. The queued task must keep it
  // alive until it runs, and `Task::run` ensures the callback observes the updated (relocated)
  // pointer.
  unsafe {
    async_global().enqueue_microtask(Task::new_gc_rooted(coro_resume_task as TaskFn, obj));
  }
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
    let resume = unsafe { (*coro).resume };
    let status = crate::ffi::invoke_coro_resume(resume, coro);
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
  coro: gc::Root,
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
      let new_node = alloc_await_reaction(node.coro.clone());
      promise_register_reaction(awaited, new_node);
      return;
    }
  };

  let coro = coro_from_obj_ptr(node.coro.ptr());
  if !coro.is_null() {
    unsafe {
      (*coro).await_is_error = await_is_error;
      (*coro).await_value = await_value;
      (*coro).await_error = await_error;
    }
    run_coroutine(coro);
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

fn alloc_await_reaction(coro: gc::Root) -> *mut crate::promise_reactions::PromiseReactionNode {
  let node = Box::new(AwaitReaction {
    node: crate::promise_reactions::PromiseReactionNode {
      next: core::ptr::null_mut(),
      vtable: &AWAIT_REACTION_VTABLE,
    },
    coro,
  });
  Box::into_raw(node) as *mut crate::promise_reactions::PromiseReactionNode
}

pub(crate) fn async_spawn(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef {
  super::ensure_event_loop_thread();
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return core::ptr::null_mut();
  }

  unsafe {
    if (*coro).promise.is_null() {
      (*coro).promise = legacy_from_promise_ref(promise_new());
    }
  }

  run_coroutine(coro);

  unsafe { (*coro).promise }
}

pub(crate) fn async_spawn_deferred(coro: *mut RtCoroutineHeader) -> LegacyPromiseRef {
  super::ensure_event_loop_thread();
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return core::ptr::null_mut();
  }

  unsafe {
    if (*coro).promise.is_null() {
      (*coro).promise = legacy_from_promise_ref(promise_new());
    }
  }

  schedule_resume_microtask(coro);

  unsafe { (*coro).promise }
}

pub(crate) fn coro_await(coro: *mut RtCoroutineHeader, awaited: LegacyPromiseRef, next_state: u32) {
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

  let awaited = promise_ref_from_legacy(awaited);

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

  let coro_root = unsafe { gc::Root::new_unchecked(coro_obj_ptr(coro)) };
  let node = alloc_await_reaction(coro_root);
  promise_register_reaction(awaited, node);
}

pub(crate) fn coro_await_value(coro: *mut RtCoroutineHeader, awaited: PromiseResolveInput, next_state: u32) {
  match awaited.kind {
    PromiseResolveKind::Promise => {
      let p: LegacyPromiseRef = unsafe { awaited.payload.promise };
      coro_await(coro, p, next_state);
    }
    PromiseResolveKind::Value | PromiseResolveKind::Thenable => {
      // Await semantics are equivalent to `PromiseResolve` + `then`.
      let p = promise_new();
      promise_resolve_into(p, awaited);
      coro_await(coro, legacy_from_promise_ref(p), next_state);
    }
  }
}
