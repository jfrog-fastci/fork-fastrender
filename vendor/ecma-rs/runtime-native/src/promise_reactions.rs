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

#[repr(C)]
struct PromiseReactionJob {
  node: *mut PromiseReactionNode,
  /// Rooted promise pointer so the job remains valid even if the promise object relocates under a
  /// moving GC.
  promise: gc::Root,
}

extern "C" fn run_promise_reaction_job(data: *mut u8) {
  // Safety: the task owns `PromiseReactionJob` via the task drop hook.
  let job = unsafe { &mut *(data as *mut PromiseReactionJob) };
  let node = job.node;
  if node.is_null() {
    return;
  }
  let vtable = unsafe { (*node).vtable };
  if vtable.is_null() {
    std::process::abort();
  }
  let promise = job.promise.ptr() as PromiseRef;
  ((unsafe { &*vtable }).run)(node, promise);
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
  ((unsafe { &*vtable }).drop)(node);
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
    ((unsafe { &*vtable }).drop)(node);
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
      ((unsafe { &*vtable }).drop)(head);
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
}
