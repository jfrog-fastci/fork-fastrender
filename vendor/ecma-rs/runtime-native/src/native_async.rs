use core::ptr::null_mut;
use core::sync::atomic::Ordering;

use crate::async_abi::{
  Coroutine, CoroutineRef, CoroutineStepTag, CoroutineVTable, PromiseHeader, PromiseState,
};
use crate::ffi::abort_on_panic;
use crate::promise_reactions::{enqueue_reaction_job, reverse_list, PromiseReactionNode, PromiseReactionVTable};
use crate::PromiseRef as AbiPromiseRef;

/// Internal promise state used while a promise is being settled.
///
/// These values are not part of the public ABI; external code should only observe
/// `PromiseHeader::{PENDING,FULFILLED,REJECTED}`.
const STATE_FULFILLING: PromiseState = 3;
const STATE_REJECTING: PromiseState = 4;

#[inline]
fn ensure_event_loop_thread_registered() {
  // The JS-shaped async runtime is driven by the main thread/event loop. Register it on
  // first use so GC can coordinate stop-the-world safepoints across all mutator threads.
  crate::threading::register_current_thread(crate::threading::ThreadKind::Main);
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
  let ptr = crate::alloc::alloc_bytes_zeroed(size, align, "rt_async_spawn: promise");
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
  (*header).state.store(PromiseHeader::PENDING, Ordering::Relaxed);
  (*header).reactions.store(0, Ordering::Relaxed);
  (*header).flags.store(0, Ordering::Relaxed);
}

fn push_reaction(promise: *mut PromiseHeader, node: *mut PromiseReactionNode) {
  let reactions = unsafe { &(*promise).reactions };
  loop {
    let head = reactions.load(Ordering::Acquire) as *mut PromiseReactionNode;
    unsafe {
      (*node).next = head;
    }
    if reactions
      .compare_exchange(head as usize, node as usize, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      break;
    }
  }
}

fn drain_reactions(promise: *mut PromiseHeader) {
  let reactions = unsafe { &(*promise).reactions };
  let mut head = reactions.swap(0, Ordering::AcqRel) as *mut PromiseReactionNode;
  if head.is_null() {
    return;
  }

  // The list is pushed in LIFO order; reverse to preserve FIFO registration order.
  head = unsafe { reverse_list(head) };

  while !head.is_null() {
    let next = unsafe { (*head).next };
    unsafe {
      (*head).next = null_mut();
    }
    enqueue_reaction_job(promise, head);
    head = next;
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
      ((unsafe { &*vtable }).drop)(node);
    }
    return;
  }

  // Mark "handled" as soon as someone attaches a reaction (await/then). This is a placeholder for
  // future unhandled rejection tracking.
  unsafe { &(*p).flags }.fetch_or(0x1, Ordering::Release);

  push_reaction(p, node);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*p).state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions(p);
  }
}

pub(crate) unsafe fn promise_fulfill(p: AbiPromiseRef) {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return;
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
    return;
  }

  state.store(PromiseHeader::FULFILLED, Ordering::Release);
  drain_reactions(header);
}

pub(crate) unsafe fn promise_reject(p: AbiPromiseRef) {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return;
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
    return;
  }

  state.store(PromiseHeader::REJECTED, Ordering::Release);
  drain_reactions(header);
}

#[repr(C)]
struct CoroutineReaction {
  node: PromiseReactionNode,
  coro: CoroutineRef,
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

fn alloc_coroutine_reaction(coro: CoroutineRef) -> *mut PromiseReactionNode {
  let node = Box::new(CoroutineReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &COROUTINE_REACTION_VTABLE,
    },
    coro,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

fn coro_await(coro: CoroutineRef, awaited: *mut PromiseHeader) {
  let awaited = validate_promise_ptr(awaited);
  if awaited.is_null() {
    return;
  }
  let node = alloc_coroutine_reaction(coro);
  promise_register_reaction(awaited, node);
}

fn run_coroutine(coro: CoroutineRef) {
  let coro = validate_coro_ptr(coro);
  if coro.is_null() {
    return;
  }

  loop {
    // Safety: `coro` is valid and properly aligned; vtable/resume pointers are provided by generated
    // code and must be valid for the coroutine's lifetime.
    let vtable_ptr = unsafe { (*coro).vtable };
    if vtable_ptr.is_null() {
      std::process::abort();
    }
    let vtable = unsafe { &*vtable_ptr };

    let step = unsafe { (vtable.resume)(coro) };
    match step.tag {
      CoroutineStepTag::Complete => return,
      CoroutineStepTag::Await => {
        let awaited = validate_promise_ptr(step.await_promise);
        if awaited.is_null() {
          return;
        }

        // Fast path: if the awaited promise is already settled, resume synchronously unless strict
        // mode is requested.
        if !crate::async_rt::strict_await_yields() {
          let state = unsafe { &(*awaited).state }.load(Ordering::Acquire);
          if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
            continue;
          }
        }

        coro_await(coro, awaited);
        return;
      }
    }
  }
}

extern "C" fn coro_resume_task(data: *mut u8) {
  let coro = data as CoroutineRef;
  run_coroutine(coro);
}

pub(crate) fn async_spawn(coro: CoroutineRef) -> AbiPromiseRef {
  abort_on_panic(|| {
    let coro = validate_coro_ptr(coro);
    if coro.is_null() {
      return AbiPromiseRef::null();
    }

    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();

    let promise = unsafe {
      if (*coro).promise.is_null() {
        let vtable_ptr = (*coro).vtable;
        if vtable_ptr.is_null() {
          std::process::abort();
        }
        let vtable = &*vtable_ptr;
        let promise = alloc_promise_for_vtable(vtable);
        (*coro).promise = promise_header_ptr(promise);
        promise
      } else {
        promise_handle_from_header((*coro).promise)
      }
    };

    run_coroutine(coro);
    promise
  })
}

pub(crate) fn async_spawn_deferred(coro: CoroutineRef) -> AbiPromiseRef {
  abort_on_panic(|| {
    let coro = validate_coro_ptr(coro);
    if coro.is_null() {
      return AbiPromiseRef::null();
    }

    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();

    let promise = unsafe {
      if (*coro).promise.is_null() {
        let vtable_ptr = (*coro).vtable;
        if vtable_ptr.is_null() {
          std::process::abort();
        }
        let vtable = &*vtable_ptr;
        let promise = alloc_promise_for_vtable(vtable);
        (*coro).promise = promise_header_ptr(promise);
        promise
      } else {
        promise_handle_from_header((*coro).promise)
      }
    };

    // Schedule the first resume as a microtask instead of running synchronously.
    crate::async_rt::enqueue_microtask(coro_resume_task, coro as *mut u8);

    promise
  })
}

