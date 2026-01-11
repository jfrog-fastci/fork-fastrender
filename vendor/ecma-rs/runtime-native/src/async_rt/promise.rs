use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::abi::{PromiseRef, ValueRef};
use crate::async_abi::PromiseHeader;
use crate::promise_reactions::{enqueue_reaction_job, reverse_list, PromiseReactionNode, PromiseReactionVTable};
use std::sync::{Condvar, Mutex};

/// Internal promise state used while a promise is being settled.
///
/// These values are not part of the public ABI; external code should only observe
/// `PromiseHeader::{PENDING,FULFILLED,REJECTED}`.
const STATE_FULFILLING: u8 = 3;
const STATE_REJECTING: u8 = 4;

#[repr(C)]
pub struct RtPromise {
  /// ABI-stable header prefix.
  pub header: PromiseHeader,
  /// Fulfillment value (valid when `header.state == FULFILLED`).
  value: AtomicUsize,
  /// Rejection reason (valid when `header.state == REJECTED`).
  error: AtomicUsize,
}

impl RtPromise {
  fn new_pending() -> Self {
    Self {
      header: PromiseHeader {
        state: core::sync::atomic::AtomicU8::new(PromiseHeader::PENDING),
        reactions: core::sync::atomic::AtomicUsize::new(0),
        flags: core::sync::atomic::AtomicU8::new(0),
      },
      value: AtomicUsize::new(0),
      error: AtomicUsize::new(0),
    }
  }
}

#[inline]
fn promise_ptr(p: PromiseRef) -> *mut RtPromise {
  if p.is_null() {
    return null_mut();
  }

  // PromiseRef is an opaque pointer handle over the ABI, but all of our promise
  // operations dereference it. Abort on misalignment to avoid UB if the ABI is
  // misused.
  let ptr = p.0 as *mut RtPromise;
  if (ptr as usize) % core::mem::align_of::<RtPromise>() != 0 {
    std::process::abort();
  }
  ptr
}

#[inline]
fn promise_header_ref(p: PromiseRef) -> crate::async_abi::PromiseRef {
  // `RtPromise` embeds `PromiseHeader` at offset 0.
  p.0.cast::<PromiseHeader>()
}

pub(crate) enum PromiseOutcome {
  Pending,
  Fulfilled(ValueRef),
  Rejected(ValueRef),
}

pub(crate) fn promise_outcome(p: PromiseRef) -> PromiseOutcome {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return PromiseOutcome::Pending;
  }

  let state = unsafe { &*ptr }.header.state.load(Ordering::Acquire);
  match state {
    PromiseHeader::FULFILLED => PromiseOutcome::Fulfilled(unsafe { &*ptr }.value.load(Ordering::Acquire) as ValueRef),
    PromiseHeader::REJECTED => PromiseOutcome::Rejected(unsafe { &*ptr }.error.load(Ordering::Acquire) as ValueRef),
    // Includes `PENDING` + internal settling states.
    _ => PromiseOutcome::Pending,
  }
}

pub(crate) fn promise_new() -> PromiseRef {
  PromiseRef(Box::into_raw(Box::new(RtPromise::new_pending())) as *mut core::ffi::c_void)
}

#[repr(C)]
struct CallbackReaction {
  node: PromiseReactionNode,
  callback: extern "C" fn(*mut u8),
  data: *mut u8,
}

extern "C" fn callback_reaction_run(node: *mut PromiseReactionNode, _promise: crate::async_abi::PromiseRef) {
  // Safety: allocated by `alloc_callback_reaction`.
  let node = unsafe { &*(node as *mut CallbackReaction) };
  (node.callback)(node.data);
}

extern "C" fn callback_reaction_drop(node: *mut PromiseReactionNode) {
  // Safety: allocated by `alloc_callback_reaction`.
  unsafe {
    drop(Box::from_raw(node as *mut CallbackReaction));
  }
}

static CALLBACK_REACTION_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
  run: callback_reaction_run,
  drop: callback_reaction_drop,
};

fn alloc_callback_reaction(callback: extern "C" fn(*mut u8), data: *mut u8) -> *mut PromiseReactionNode {
  let node = Box::new(CallbackReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &CALLBACK_REACTION_VTABLE,
    },
    callback,
    data,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

fn push_reaction(ptr: *mut RtPromise, node: *mut PromiseReactionNode) {
  let reactions = unsafe { &(*ptr).header.reactions };
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

fn drain_reactions(ptr: *mut RtPromise) {
  let reactions = unsafe { &(*ptr).header.reactions };
  let mut head = reactions.swap(0, Ordering::AcqRel) as *mut PromiseReactionNode;
  if head.is_null() {
    return;
  }

  // The list is pushed in LIFO order; reverse to preserve FIFO registration order.
  head = unsafe { reverse_list(head) };

  let promise = ptr.cast::<PromiseHeader>();
  while !head.is_null() {
    let next = unsafe { (*head).next };
    unsafe {
      (*head).next = null_mut();
    }
    enqueue_reaction_job(promise, head);
    head = next;
  }
}

/// Register a reaction node on a promise.
///
/// This is the unified internal mechanism used by both `await` and `then`-style APIs.
pub(crate) fn promise_register_reaction(p: PromiseRef, node: *mut PromiseReactionNode) {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
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

  // Test-only deterministic race hook: allow a resolver thread to settle/drain while this
  // registration is paused before linking into `reactions`.
  if let Some(hook) = debug_waiter_race_hook() {
    let state = unsafe { &(*ptr).header.state }.load(Ordering::Acquire);
    if state == PromiseHeader::PENDING {
      hook.notify_waiter_checked_pending();
      hook.wait_for_resolved();
    }
  }

  // Mark "handled" as soon as someone attaches a reaction (await/then). This is a placeholder for
  // future unhandled rejection tracking.
  unsafe { &(*ptr).header.flags }.fetch_or(0x1, Ordering::Release);

  push_reaction(ptr, node);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*ptr).header.state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions(ptr);
  }
}

pub(crate) fn promise_then(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  if p.is_null() {
    // Treat null as "never settles": keep it pending.
    return;
  }
  let node = alloc_callback_reaction(on_settle, data);
  promise_register_reaction(p, node);
}

pub(crate) fn promise_resolve(p: PromiseRef, value: ValueRef) {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return;
  }

  let hook = debug_waiter_race_hook();
  if let Some(hook) = hook {
    hook.wait_for_waiter_checked_pending();
  }

  let state = unsafe { &(*ptr).header.state };
  if state
    .compare_exchange(
      PromiseHeader::PENDING,
      STATE_FULFILLING,
      Ordering::AcqRel,
      Ordering::Acquire,
    )
    .is_err()
  {
    if let Some(hook) = hook {
      hook.notify_resolved();
    }
    return;
  }

  // Publish the result before flipping to the externally-visible fulfilled state.
  unsafe { &(*ptr).value }.store(value as usize, Ordering::Relaxed);
  unsafe { &(*ptr).error }.store(0, Ordering::Relaxed);
  state.store(PromiseHeader::FULFILLED, Ordering::Release);

  drain_reactions(ptr);

  if let Some(hook) = hook {
    hook.notify_resolved();
  }
}

pub(crate) fn promise_reject(p: PromiseRef, err: ValueRef) {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return;
  }

  let hook = debug_waiter_race_hook();
  if let Some(hook) = hook {
    hook.wait_for_waiter_checked_pending();
  }

  let state = unsafe { &(*ptr).header.state };
  if state
    .compare_exchange(
      PromiseHeader::PENDING,
      STATE_REJECTING,
      Ordering::AcqRel,
      Ordering::Acquire,
    )
    .is_err()
  {
    if let Some(hook) = hook {
      hook.notify_resolved();
    }
    return;
  }

  unsafe { &(*ptr).error }.store(err as usize, Ordering::Relaxed);
  unsafe { &(*ptr).value }.store(0, Ordering::Relaxed);
  state.store(PromiseHeader::REJECTED, Ordering::Release);

  drain_reactions(ptr);

  if let Some(hook) = hook {
    hook.notify_resolved();
  }
}

/// Debug/test-only helper: expose the raw header pointer for a promise handle.
#[allow(dead_code)]
pub(crate) fn promise_header(p: PromiseRef) -> crate::async_abi::PromiseRef {
  promise_header_ref(p)
}

// -----------------------------------------------------------------------------
// Test hooks / debug helpers (not stable API)
// -----------------------------------------------------------------------------

pub(crate) struct PromiseWaiterRaceHook {
  stage: Mutex<u8>,
  cv: Condvar,
}

impl PromiseWaiterRaceHook {
  pub(crate) fn new() -> Self {
    Self {
      stage: Mutex::new(0),
      cv: Condvar::new(),
    }
  }

  fn notify_waiter_checked_pending(&self) {
    let mut stage = self.stage.lock().unwrap();
    *stage = 1;
    self.cv.notify_all();
  }

  fn wait_for_resolved(&self) {
    let mut stage = self.stage.lock().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while *stage < 2 {
      let now = std::time::Instant::now();
      if now >= deadline {
        panic!("timed out waiting for promise to be resolved during race hook");
      }
      let timeout = deadline - now;
      let (guard, _) = self.cv.wait_timeout(stage, timeout).unwrap();
      stage = guard;
    }
  }

  fn wait_for_waiter_checked_pending(&self) {
    let mut stage = self.stage.lock().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while *stage < 1 {
      let now = std::time::Instant::now();
      if now >= deadline {
        panic!("timed out waiting for waiter registration during race hook");
      }
      let timeout = deadline - now;
      let (guard, _) = self.cv.wait_timeout(stage, timeout).unwrap();
      stage = guard;
    }
  }

  fn notify_resolved(&self) {
    let mut stage = self.stage.lock().unwrap();
    *stage = 2;
    self.cv.notify_all();
  }
}

static DEBUG_WAITER_RACE_HOOK: AtomicPtr<PromiseWaiterRaceHook> = AtomicPtr::new(core::ptr::null_mut());

pub(crate) fn debug_set_waiter_race_hook(hook: Option<&'static PromiseWaiterRaceHook>) {
  let ptr = hook
    .map(|h| h as *const PromiseWaiterRaceHook as *mut PromiseWaiterRaceHook)
    .unwrap_or(core::ptr::null_mut());
  DEBUG_WAITER_RACE_HOOK.store(ptr, Ordering::Release);
}

fn debug_waiter_race_hook() -> Option<&'static PromiseWaiterRaceHook> {
  let ptr = DEBUG_WAITER_RACE_HOOK.load(Ordering::Acquire);
  if ptr.is_null() {
    None
  } else {
    // Safety: the hook is set only from tests and is expected to live for the duration of the test.
    Some(unsafe { &*ptr })
  }
}

pub(crate) fn debug_waiters_is_empty(p: PromiseRef) -> bool {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return true;
  }
  unsafe { &(*ptr).header.reactions }.load(Ordering::Acquire) == 0
}
