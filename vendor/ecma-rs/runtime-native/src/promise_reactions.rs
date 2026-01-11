//! Promise reaction infrastructure shared by `await` and `then`-style callbacks.
//!
//! This is intentionally "spec-shaped": everything that happens after a promise settles is
//! represented as a *reaction* node scheduled onto the microtask queue.
//!
//! Reactions are stored in an intrusive list (`PromiseReactionNode::next`). The promise runtime
//! stores a single pointer to the list head and drains it on settlement (reversing the list to
//! preserve FIFO registration order).
//!
//! A reaction node is executed via a vtable so the runtime can schedule heterogeneous reactions
//! uniformly (coroutine resumes, `then` callbacks, etc).

use core::ptr::null_mut;

use crate::async_abi::PromiseRef;
use crate::async_rt::gc;
use crate::async_rt::{global as async_global, Task};

/// VTable for a promise reaction node.
#[repr(C)]
pub struct PromiseReactionVTable {
  /// Execute the reaction for `promise` (which is known to be already-settled).
  pub run: extern "C" fn(node: *mut PromiseReactionNode, promise: PromiseRef),
  /// Destroy/free `node` without executing it.
  pub drop: extern "C" fn(node: *mut PromiseReactionNode),
}

/// Base header for all promise reaction nodes.
///
/// Concrete node types must embed this as the first field so they can be cast to/from
/// `PromiseReactionNode` using a simple pointer cast.
#[repr(C)]
pub struct PromiseReactionNode {
  pub next: *mut PromiseReactionNode,
  pub vtable: *const PromiseReactionVTable,
}

/// Decode a raw `PromiseHeader.waiters` value into a reaction-node pointer.
///
/// `PromiseHeader.waiters` is specified to contain either:
/// - `0`, or
/// - a valid, aligned `PromiseReactionNode*` cast to `usize`.
///
/// Any other value is treated as ABI misuse / memory corruption and terminates the process with
/// `abort()` to avoid dereferencing an invalid pointer (UB).
#[inline]
pub(crate) fn decode_waiters_ptr(head_val: usize) -> *mut PromiseReactionNode {
  if head_val == 0 {
    return null_mut();
  }
  if head_val % core::mem::align_of::<PromiseReactionNode>() != 0 {
    std::process::abort();
  }
  head_val as *mut PromiseReactionNode
}

#[repr(C)]
struct PromiseReactionJob {
  node: *mut PromiseReactionNode,
  /// Rooted promise pointer so the job remains valid even if the promise object relocates under a
  /// moving GC.
  promise: gc::Root,
}

extern "C" fn run_promise_reaction_job(data: *mut u8) {
  // Safety: `data` was allocated by `Box::into_raw(PromiseReactionJob)` in `make_reaction_task` and
  // is freed by the task drop hook (`drop_promise_reaction_job`) after this callback returns.
  let job = unsafe { &mut *(data as *mut PromiseReactionJob) };
  let node = job.node;
  if node.is_null() {
    return;
  }
  let vtable = unsafe { (*node).vtable };
  if vtable.is_null() {
    std::process::abort();
  }
  let vtable = unsafe { &*vtable };
  let promise = job.promise.ptr() as PromiseRef;
  crate::ffi::abort_on_callback_panic(|| unsafe {
    let run: extern "C-unwind" fn(*mut PromiseReactionNode, PromiseRef) =
      std::mem::transmute((&*vtable).run);
    run(node, promise);
  });
}

extern "C" fn drop_promise_reaction_job(data: *mut u8) {
  // Safety: `data` was allocated by `Box::into_raw(PromiseReactionJob)` in
  // `make_reaction_task`.
  let job = unsafe { Box::from_raw(data as *mut PromiseReactionJob) };
  let node = job.node;
  if node.is_null() {
    return;
  }
  let vtable = unsafe { (*node).vtable };
  if vtable.is_null() {
    std::process::abort();
  }
  crate::ffi::abort_on_callback_panic(|| unsafe {
    let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
    drop_fn(node);
  });
}

fn make_reaction_task(node: *mut PromiseReactionNode, promise: gc::Root) -> Task {
  let job = Box::new(PromiseReactionJob { node, promise });
  Task::new_with_drop(
    run_promise_reaction_job,
    Box::into_raw(job) as *mut u8,
    drop_promise_reaction_job,
  )
}

/// Enqueue `node` as a microtask to run for `promise`.
///
/// This is used when registering a reaction against an already-settled promise.
pub(crate) fn enqueue_reaction_job(promise: PromiseRef, node: *mut PromiseReactionNode) {
  if node.is_null() {
    return;
  }

  if promise.is_null() {
    // Treat null as "never settles": discard the node so it doesn't leak.
    let vtable = unsafe { (*node).vtable };
    if vtable.is_null() {
      std::process::abort();
    }
    crate::ffi::abort_on_callback_panic(|| unsafe {
      let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
      drop_fn(node);
    });
    return;
  }

  let promise_root = unsafe { gc::Root::new_unchecked(promise.cast::<u8>()) };
  async_global().enqueue_microtask(make_reaction_task(node, promise_root));
}

/// Enqueue a linked list of reaction nodes as microtasks in a single queue operation.
///
/// This is used by promise settlement paths that may enqueue many reactions at once (e.g. many
/// coroutines awaiting the same promise). Batching avoids a race where an event-loop thread wakes on
/// the first enqueued microtask and drains the queue faster than another thread can enqueue the
/// remaining reaction jobs.
pub(crate) fn enqueue_reaction_jobs(promise: PromiseRef, mut head: *mut PromiseReactionNode) {
  if head.is_null() {
    return;
  }

  // Fast path: if there is only one reaction, reuse the single-node enqueue helper.
  let next = unsafe { (*head).next };
  if next.is_null() {
    enqueue_reaction_job(promise, head);
    return;
  }

  if promise.is_null() {
    // Treat null as "never settles": discard the whole list so it doesn't leak.
    while !head.is_null() {
      let next = unsafe { (*head).next };
      unsafe {
        (*head).next = null_mut();
      }
      let vtable = unsafe { (*head).vtable };
      if vtable.is_null() {
        std::process::abort();
      }
      crate::ffi::abort_on_callback_panic(|| unsafe {
        let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) =
          std::mem::transmute((&*vtable).drop);
        drop_fn(head);
      });
      head = next;
    }
    return;
  }

  // Root the promise once and clone the handle into each task so the promise remains valid even if
  // it relocates under a moving GC while the reaction jobs are queued.
  //
  // This also avoids N separate handle-table allocations when draining many reactions at once.
  let promise_root = unsafe { gc::Root::new_unchecked(promise.cast::<u8>()) };
  let mut tasks: Vec<Task> = Vec::new();
  while !head.is_null() {
    let next = unsafe { (*head).next };
    unsafe {
      (*head).next = null_mut();
    }
    let node = head;
    tasks.push(make_reaction_task(node, promise_root.clone()));
    head = next;
  }

  async_global().enqueue_microtasks(tasks);
}

/// Reverse an intrusive singly-linked list in place.
///
/// # Safety
/// `head` must point to a valid list of [`PromiseReactionNode`] objects.
pub(crate) unsafe fn reverse_list(mut head: *mut PromiseReactionNode) -> *mut PromiseReactionNode {
  let mut prev: *mut PromiseReactionNode = null_mut();
  while !head.is_null() {
    let next = (*head).next;
    (*head).next = prev;
    prev = head;
    head = next;
  }
  prev
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use super::*;

  #[repr(C)]
  struct ObservePromiseNode {
    header: PromiseReactionNode,
    observed_promise: *const AtomicUsize,
  }

  extern "C" fn observe_promise_run(node: *mut PromiseReactionNode, promise: PromiseRef) {
    let node = node.cast::<ObservePromiseNode>();
    unsafe {
      (*(*node).observed_promise).store(promise as usize, Ordering::SeqCst);
    }
  }

  extern "C" fn observe_promise_drop(node: *mut PromiseReactionNode) {
    unsafe {
      drop(Box::from_raw(node.cast::<ObservePromiseNode>()));
    }
  }

  static OBSERVE_PROMISE_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
    run: observe_promise_run,
    drop: observe_promise_drop,
  };

  fn make_observe_promise_node(observed_promise: *const AtomicUsize) -> *mut PromiseReactionNode {
    Box::into_raw(Box::new(ObservePromiseNode {
      header: PromiseReactionNode {
        next: null_mut(),
        vtable: &OBSERVE_PROMISE_VTABLE,
      },
      observed_promise,
    }))
    .cast::<PromiseReactionNode>()
  }

  #[repr(C)]
  struct TestNode {
    header: PromiseReactionNode,
    drops: *const AtomicUsize,
    bad_next: *const AtomicUsize,
  }

  extern "C" fn test_run(_node: *mut PromiseReactionNode, _promise: PromiseRef) {
    std::process::abort();
  }

  extern "C" fn test_drop(node: *mut PromiseReactionNode) {
    let node = node.cast::<TestNode>();
    unsafe {
      if !(*node).header.next.is_null() {
        (*(*node).bad_next).fetch_add(1, Ordering::SeqCst);
      }
      (*(*node).drops).fetch_add(1, Ordering::SeqCst);
      drop(Box::from_raw(node));
    }
  }

  static TEST_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
    run: test_run,
    drop: test_drop,
  };

  fn make_node(
    next: *mut PromiseReactionNode,
    drops: *const AtomicUsize,
    bad_next: *const AtomicUsize,
  ) -> *mut PromiseReactionNode {
    Box::into_raw(Box::new(TestNode {
      header: PromiseReactionNode {
        next,
        vtable: &TEST_VTABLE,
      },
      drops,
      bad_next,
    }))
    .cast::<PromiseReactionNode>()
  }

  #[test]
  fn enqueue_reaction_jobs_drops_nodes_when_promise_is_null() {
    let drops = Box::new(AtomicUsize::new(0));
    let bad_next = Box::new(AtomicUsize::new(0));
    let drops_ptr: *const AtomicUsize = &*drops;
    let bad_next_ptr: *const AtomicUsize = &*bad_next;

    let n3 = make_node(null_mut(), drops_ptr, bad_next_ptr);
    let n2 = make_node(n3, drops_ptr, bad_next_ptr);
    let n1 = make_node(n2, drops_ptr, bad_next_ptr);

    enqueue_reaction_jobs(null_mut(), n1);

    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert_eq!(bad_next.load(Ordering::SeqCst), 0);
  }

  #[test]
  fn reaction_job_uses_root_ptr_when_promise_relocates() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    let observed = AtomicUsize::new(0);
    let node = make_observe_promise_node(&observed);

    // Use stable, non-null, correctly-aligned addresses as dummy "promise" pointers; the reaction
    // job treats them as opaque.
    let promise1_alloc = Box::into_raw(Box::new(1u64));
    let promise2_alloc = Box::into_raw(Box::new(2u64));
    let promise1: PromiseRef = promise1_alloc.cast();
    let promise2: PromiseRef = promise2_alloc.cast();

    let promise_root = unsafe { gc::Root::new_unchecked(promise1.cast::<u8>()) };
    let id = promise_root.id();

    // Simulate a moving GC relocating the promise object by rewriting the persistent handle table
    // entry. `run_promise_reaction_job` must read from the root handle and observe `promise2`.
    assert!(crate::roots::global_persistent_handle_table().set(id, promise2.cast::<u8>()));

    let job = Box::new(PromiseReactionJob {
      node,
      promise: promise_root,
    });
    let job_ptr = Box::into_raw(job).cast::<u8>();
    run_promise_reaction_job(job_ptr);
    drop_promise_reaction_job(job_ptr);

    assert_eq!(observed.load(Ordering::SeqCst), promise2 as usize);

    // The async GC root only keeps the address alive; free our dummy allocations explicitly.
    unsafe {
      drop(Box::from_raw(promise1_alloc));
      drop(Box::from_raw(promise2_alloc));
    }
  }

  #[test]
  fn enqueue_reaction_jobs_roots_promise_once_for_batched_jobs() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    let drops = Box::new(AtomicUsize::new(0));
    let bad_next = Box::new(AtomicUsize::new(0));
    let drops_ptr: *const AtomicUsize = &*drops;
    let bad_next_ptr: *const AtomicUsize = &*bad_next;

    let n2 = make_node(null_mut(), drops_ptr, bad_next_ptr);
    let n1 = make_node(n2, drops_ptr, bad_next_ptr);

    let mut base_handles = 0usize;
    crate::roots::global_persistent_handle_table().for_each_root_slot(|_| base_handles += 1);

    let promise_alloc = Box::into_raw(Box::new(crate::test_util::new_promise_header_pending()));
    let promise: PromiseRef = promise_alloc;
    enqueue_reaction_jobs(promise, n1);

    let mut handles_after_enqueue = 0usize;
    crate::roots::global_persistent_handle_table().for_each_root_slot(|_| handles_after_enqueue += 1);
    assert_eq!(
      handles_after_enqueue,
      base_handles + 1,
      "batched reaction jobs must share a single persistent handle for the promise"
    );

    // Drop queued microtasks; this should drop both reaction nodes and release the persistent handle.
    crate::async_rt::clear_state_for_tests();

    let mut handles_after_clear = 0usize;
    crate::roots::global_persistent_handle_table().for_each_root_slot(|_| handles_after_clear += 1);
    assert_eq!(handles_after_clear, base_handles);

    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(bad_next.load(Ordering::SeqCst), 0);

    unsafe {
      drop(Box::from_raw(promise_alloc));
    }
  }
}
