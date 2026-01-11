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
  promise: PromiseRef,
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
  ((unsafe { &*vtable }).run)(node, job.promise);
}

extern "C" fn drop_promise_reaction_job(data: *mut u8) {
  // Safety: `data` was allocated by `Box::into_raw(PromiseReactionJob)` in
  // `enqueue_reaction_job`.
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

/// Enqueue `node` as a microtask to run for `promise`.
///
/// This is the shared scheduling primitive used by both promise settlement and registrations that
/// attach to an already-settled promise.
pub(crate) fn enqueue_reaction_job(promise: PromiseRef, node: *mut PromiseReactionNode) {
  if node.is_null() {
    return;
  }
  let job = Box::new(PromiseReactionJob { node, promise });
  async_global().enqueue_microtask(Task::new_with_drop(
    run_promise_reaction_job,
    Box::into_raw(job) as *mut u8,
    drop_promise_reaction_job,
  ));
}

/// Enqueue a linked list of reactions as microtasks in a single queue operation.
///
/// This is intended for promise settlement paths that may enqueue many reactions at once (e.g. many
/// coroutines awaiting the same promise). Batching avoids a race where an event-loop thread wakes on
/// the first enqueued microtask and drains the queue faster than another thread can enqueue the
/// remaining reaction jobs.
pub(crate) fn enqueue_reaction_jobs(promise: PromiseRef, mut head: *mut PromiseReactionNode) {
  if head.is_null() {
    return;
  }

  let mut tasks: Vec<Task> = Vec::new();
  while !head.is_null() {
    let next = unsafe { (*head).next };
    unsafe {
      (*head).next = null_mut();
    }

    let node = head;
    let job = Box::new(PromiseReactionJob { node, promise });
    tasks.push(Task::new_with_drop(
      run_promise_reaction_job,
      Box::into_raw(job) as *mut u8,
      drop_promise_reaction_job,
    ));

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
