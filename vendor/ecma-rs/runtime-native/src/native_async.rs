use core::ptr::null_mut;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::async_abi::{
  Coroutine, CoroutineRef, CoroutineStepTag, CoroutineVTable, PromiseHeader, PromiseState, CORO_FLAG_RUNTIME_OWNS_FRAME,
  PROMISE_FLAG_EXTERNAL_PENDING,
};
use crate::async_rt::Task;
use crate::ffi::abort_on_panic;
use crate::CoroutineId;
use crate::promise_reactions::{
  decode_waiters_ptr, enqueue_reaction_jobs, reverse_list, PromiseReactionNode, PromiseReactionVTable,
};
use crate::PromiseRef as AbiPromiseRef;

/// Internal promise state used while a promise is being settled.
///
/// These values are not part of the public ABI; external code should only observe
/// `PromiseHeader::{PENDING,FULFILLED,REJECTED}`.
const STATE_FULFILLING: PromiseState = 3;
const STATE_REJECTING: PromiseState = 4;

#[inline]
fn ensure_event_loop_thread_registered() {
  // The native async ABI is driven by the JS-shaped `async_rt` event loop. Ensure the current
  // thread is registered with the appropriate kind:
  // - `Main` for the event-loop thread
  // - `External` for other threads that enter the runtime
  //
  // This matches the behavior used by `rt_async_poll` and avoids permanently "upgrading" an
  // arbitrary thread to `Main` (thread kinds are monotonic).
  crate::async_rt::ensure_event_loop_thread();
}

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

// -----------------------------------------------------------------------------
// Coroutine handle resolution
// -----------------------------------------------------------------------------
//
// `CoroutineId` is an ABI-stable `u64` handle to a coroutine frame. The native async runtime must
// not store raw `Coroutine*` pointers across any async boundary (microtask queues, promise reaction
// lists, OS event-loop userdata, cross-thread wakeups, ...): a moving/compacting GC may relocate the
// coroutine frame, making previously captured pointers stale.
//
// Coroutines are resolved via the persistent handle ABI (`rt_handle_load` / `rt_handle_free`). The
// handle table is GC-aware: the GC updates the underlying slot when the coroutine frame moves.

#[inline]
fn coro_load(id: CoroutineId) -> Option<CoroutineRef> {
  let ptr = crate::rt_handle_load(id.0);
  let coro = validate_coro_ptr(ptr.cast::<Coroutine>());
  (!coro.is_null()).then_some(coro)
}

#[inline]
fn validate_promise_ptr(p: *mut PromiseHeader) -> *mut PromiseHeader {
  if p.is_null() {
    return p;
  }
  if (p as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  p
}

#[inline]
fn promise_header_ptr(p: AbiPromiseRef) -> *mut PromiseHeader {
  validate_promise_ptr(p.0.cast::<PromiseHeader>())
}

#[inline]
fn promise_handle_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast())
}

fn alloc_promise_for_vtable(vtable: &CoroutineVTable) -> AbiPromiseRef {
  let size = vtable.promise_size as usize;
  let align = vtable.promise_align as usize;
  if size < core::mem::size_of::<PromiseHeader>() {
    std::process::abort();
  }
  if align < core::mem::align_of::<PromiseHeader>() || !align.is_power_of_two() {
    std::process::abort();
  }
  if !vtable.promise_shape_id.is_valid() {
    std::process::abort();
  }

  // Allocate the promise as a normal GC object so it can be traced/moved in the future.
  // The returned pointer is the GC object base (points at ObjHeader), which is also the start of
  // `PromiseHeader` (it embeds ObjHeader at offset 0).
  let ptr = crate::rt_alloc(size, vtable.promise_shape_id);
  let p = AbiPromiseRef(ptr.cast());
  unsafe {
    promise_init(p);
  }
  p
}

pub(crate) unsafe fn promise_init(p: AbiPromiseRef) {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return;
  }
  // Initialize to a clean pending state.
  //
  // SAFETY: callers may pass freshly-allocated (uninitialized) memory for the promise header. Use
  // `addr_of_mut(...).write(...)` to initialize atomic fields without creating references to
  // uninitialized `Atomic*` values.
  unsafe {
    core::ptr::addr_of_mut!((*header).state).write(AtomicU8::new(PromiseHeader::PENDING));
    core::ptr::addr_of_mut!((*header).waiters).write(AtomicUsize::new(0));
    core::ptr::addr_of_mut!((*header).flags).write(AtomicU8::new(0));
  }
}

fn push_reaction(promise: *mut PromiseHeader, node: *mut PromiseReactionNode) {
  let reactions = unsafe { &(*promise).waiters };
  loop {
    let head_val = reactions.load(Ordering::Acquire);
    let head = decode_waiters_ptr(head_val);
    unsafe {
      (*node).next = head;
    }
    if reactions
      .compare_exchange(head_val, node as usize, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      break;
    }
  }
}

fn drain_reactions(promise: *mut PromiseHeader) {
  let reactions = unsafe { &(*promise).waiters };
  let head_val = reactions.swap(0, Ordering::AcqRel);
  let mut head = decode_waiters_ptr(head_val);
  if head.is_null() {
    // No more reactions; ensure we don't retain the promise in the tracking set.
    crate::async_rt::promise::untrack_pending_reactions(promise);
    return;
  }

  // The promise no longer owns any pending reactions, so it can be removed from the tracking set
  // even before we schedule the drained list.
  crate::async_rt::promise::untrack_pending_reactions(promise);

  // The list is pushed in LIFO order; reverse to preserve FIFO registration order.
  head = unsafe { reverse_list(head) };

  enqueue_reaction_jobs(promise, head);
}

fn promise_mark_handled(p: *mut PromiseHeader) {
  let p = validate_promise_ptr(p);
  if p.is_null() {
    return;
  }

  // `await` / `then` attaches a handler. Track the first transition so we can emit a
  // `rejectionhandled` notification if the promise was previously reported as unhandled.
  let transitioned = unsafe { (*p).mark_handled() };
  if transitioned {
    crate::unhandled_rejection::on_handle(promise_handle_from_header(p));
  }
}

fn promise_register_reaction(p: *mut PromiseHeader, node: *mut PromiseReactionNode) {
  let p = validate_promise_ptr(p);
  if p.is_null() {
    // Treat null as "never settles": discard the node so it doesn't leak.
    if !node.is_null() {
      let vtable = unsafe { (*node).vtable };
      if vtable.is_null() {
        std::process::abort();
      }
      crate::ffi::abort_on_callback_panic(|| unsafe {
        let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
        drop_fn(node);
      });
    }
    return;
  }

  // Mark "handled" as soon as someone attaches a reaction (await/then).
  promise_mark_handled(p);

  push_reaction(p, node);
  crate::async_rt::promise::track_pending_reactions(p);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*p).state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions(p);
  }
}

#[inline]
fn maybe_clear_external_pending(promise: *mut PromiseHeader) {
  let promise = validate_promise_ptr(promise);
  if promise.is_null() {
    return;
  }

  let prev = unsafe { &(*promise).flags }.fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel);
  if (prev & PROMISE_FLAG_EXTERNAL_PENDING) != 0 {
    crate::async_rt::external_pending_dec();
  }
}

pub(crate) unsafe fn promise_try_fulfill(p: AbiPromiseRef) -> bool {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return false;
  }

  let state = &(*header).state;
  if state
    .compare_exchange(
      PromiseHeader::PENDING,
      STATE_FULFILLING,
      Ordering::AcqRel,
      Ordering::Acquire,
    )
    .is_err()
  {
    maybe_clear_external_pending(header);
    return false;
  }

  state.store(PromiseHeader::FULFILLED, Ordering::Release);
  drain_reactions(header);
  maybe_clear_external_pending(header);
  true
}

pub(crate) unsafe fn promise_fulfill(p: AbiPromiseRef) {
  let _ = unsafe { promise_try_fulfill(p) };
}

pub(crate) unsafe fn promise_try_reject(p: AbiPromiseRef) -> bool {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return false;
  }

  let state = &(*header).state;
  if state
    .compare_exchange(
      PromiseHeader::PENDING,
      STATE_REJECTING,
      Ordering::AcqRel,
      Ordering::Acquire,
    )
    .is_err()
  {
    maybe_clear_external_pending(header);
    return false;
  }

  state.store(PromiseHeader::REJECTED, Ordering::Release);

  // If no one attached a handler yet, schedule unhandled-rejection tracking.
  if unsafe { !(*header).is_handled() } {
    crate::unhandled_rejection::on_reject(p);
  }
  drain_reactions(header);
  maybe_clear_external_pending(header);
  true
}

pub(crate) unsafe fn promise_reject(p: AbiPromiseRef) {
  let _ = unsafe { promise_try_reject(p) };
}

#[repr(C)]
struct CoroutineReaction {
  node: PromiseReactionNode,
  coro: CoroutineId,
}

extern "C" fn coroutine_reaction_run(node: *mut PromiseReactionNode, _promise: *mut PromiseHeader) {
  let node = node as *mut CoroutineReaction;
  if node.is_null() {
    return;
  }
  let coro = unsafe { (*node).coro };
  run_coroutine(coro);
}

extern "C" fn coroutine_reaction_drop(node: *mut PromiseReactionNode) {
  if node.is_null() {
    return;
  }
  unsafe {
    drop(Box::from_raw(node as *mut CoroutineReaction));
  }
}

static COROUTINE_REACTION_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
  run: coroutine_reaction_run,
  drop: coroutine_reaction_drop,
};

fn alloc_coroutine_reaction(coro: CoroutineId) -> *mut PromiseReactionNode {
  let node = Box::new(CoroutineReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &COROUTINE_REACTION_VTABLE,
    },
    coro,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

fn coro_await(coro: CoroutineId, awaited: *mut PromiseHeader) {
  let awaited = validate_promise_ptr(awaited);
  if awaited.is_null() {
    return;
  }
  let node = alloc_coroutine_reaction(coro);
  if node.is_null() {
    return;
  }
  promise_register_reaction(awaited, node);
}

fn run_coroutine(coro_id: CoroutineId) {
  loop {
    let Some(coro) = coro_load(coro_id) else {
      // Invalid/stale handle: treat as a no-op resume (must not UB).
      if cfg!(debug_assertions) && coro_id.0 != 0 {
        // Don't panic/unwind across FFI; print a debug hint so handle misuse is visible.
        eprintln!("native async: attempted to resume invalid CoroutineId({})", coro_id.0);
      }
      return;
    };

    // Safety: `coro` is valid and properly aligned; vtable/resume pointers are provided by generated
    // code and must be valid for the coroutine's lifetime.
    let vtable_ptr = unsafe { (*coro).vtable };
    if vtable_ptr.is_null() {
      std::process::abort();
    }
    let vtable = unsafe { &*vtable_ptr };

    // Capture flags before calling into generated code. The resume function may safepoint and the
    // coroutine frame may move, so we must not dereference `coro` afterwards.
    let flags = unsafe { (*coro).flags };

    let step = crate::ffi::abort_on_callback_panic(|| unsafe {
      let resume: unsafe extern "C-unwind" fn(CoroutineRef) -> crate::async_abi::CoroutineStep =
        std::mem::transmute(vtable.resume);
      resume(coro)
    });
    match step.tag {
      CoroutineStepTag::Complete => {
        // The runtime owns the coroutine handle after spawn. On completion we:
        // - destroy the coroutine frame if runtime-owned, and
        // - free the stable handle.
        crate::async_runtime::coro_destroy_once(coro_id);
        return;
      }
      CoroutineStepTag::Await => {
        // A coroutine that yields must be stored across turns (await reaction + later resume).
        // Stack-owned frames cannot outlive the spawning call and would otherwise cause
        // use-after-return UB.
        if cfg!(debug_assertions) && (flags & CORO_FLAG_RUNTIME_OWNS_FRAME) == 0 {
          eprintln!(
            "runtime-native async ABI violation: coroutine yielded `Await` but \
CORO_FLAG_RUNTIME_OWNS_FRAME was not set (stack-owned coroutine frames must not suspend)"
          );
          std::process::abort();
        }

        let awaited = validate_promise_ptr(step.await_promise);
        if awaited.is_null() {
          return;
        }

        // `await` counts as attaching a rejection handler. This must happen even when taking the
        // settled fast path (synchronous resumption), because attaching handlers after rejection
        // should trigger `rejectionhandled` behavior.
        promise_mark_handled(awaited);

        // Fast path: if the awaited promise is already settled, resume synchronously unless strict
        // mode is requested.
        if !crate::async_rt::strict_await_yields() {
          let state = unsafe { &(*awaited).state }.load(Ordering::Acquire);
          if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
            continue;
          }
        }

        coro_await(coro_id, awaited);
        return;
      }
    }
  }
}

extern "C" fn coro_resume_task(data: *mut u8) {
  if data.is_null() {
    return;
  }
  // Safety: `data` is a `Box<CoroutineId>` owned by the task.
  let coro = unsafe { *(data as *const CoroutineId) };
  run_coroutine(coro);
}

extern "C" fn coro_resume_task_drop(data: *mut u8) {
  if data.is_null() {
    return;
  }
  // Safety: `data` was allocated by `Box::into_raw(Box::new(CoroutineId(..)))`.
  unsafe {
    drop(Box::from_raw(data as *mut CoroutineId));
  }
}

pub(crate) fn async_spawn(coro: CoroutineId) -> AbiPromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::async_runtime::track_coro_if_runtime_owned(coro);

    let Some(coro_ptr) = coro_load(coro) else {
      return AbiPromiseRef::null();
    };
    unsafe {
      crate::validate_async_abi_coro_vtable(coro_ptr);
    }

    let promise = unsafe {
      if (*coro_ptr).promise.is_null() {
        let vtable_ptr = (*coro_ptr).vtable;
        if vtable_ptr.is_null() {
          std::process::abort();
        }
        let vtable = &*vtable_ptr;
        let promise = alloc_promise_for_vtable(vtable);
        if let Some(coro_ptr) = coro_load(coro) {
          (*coro_ptr).promise = promise_header_ptr(promise);
        }
        promise
      } else {
        promise_handle_from_header((*coro_ptr).promise)
      }
    };

    run_coroutine(coro);
    promise
  })
}

pub(crate) fn async_spawn_deferred(coro: CoroutineId) -> AbiPromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();

    let Some(coro_ptr) = coro_load(coro) else {
      return AbiPromiseRef::null();
    };
    unsafe {
      crate::validate_async_abi_coro_vtable(coro_ptr);
    }

    if cfg!(debug_assertions) && unsafe { (*coro_ptr).flags & CORO_FLAG_RUNTIME_OWNS_FRAME } == 0 {
      eprintln!(
        "runtime-native async ABI violation: rt_async_spawn_deferred was called with a \
stack-owned coroutine frame (CORO_FLAG_RUNTIME_OWNS_FRAME must be set)"
      );
      std::process::abort();
    }
    crate::async_runtime::track_coro_if_runtime_owned(coro);

    let promise = unsafe {
      if (*coro_ptr).promise.is_null() {
        let vtable_ptr = (*coro_ptr).vtable;
        if vtable_ptr.is_null() {
          std::process::abort();
        }
        let vtable = &*vtable_ptr;
        let promise = alloc_promise_for_vtable(vtable);
        if let Some(coro_ptr) = coro_load(coro) {
          (*coro_ptr).promise = promise_header_ptr(promise);
        }
        promise
      } else {
        promise_handle_from_header((*coro_ptr).promise)
      }
    };

    // Schedule the first resume as a microtask instead of running synchronously.
    let data = Box::into_raw(Box::new(coro)) as *mut u8;
    crate::async_rt::global().enqueue_microtask(Task::new_with_drop(
      coro_resume_task,
      data,
      coro_resume_task_drop,
    ));

    promise
  })
}
