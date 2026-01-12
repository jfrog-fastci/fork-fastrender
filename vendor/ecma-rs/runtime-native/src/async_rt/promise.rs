use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::abi::{PromiseRef, PromiseResolveInput, PromiseResolveKind, ThenableRef, ValueRef};
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING, PROMISE_FLAG_HAS_PAYLOAD};
use crate::sync::GcAwareMutex;
use once_cell::sync::Lazy;
use std::collections::HashSet;
use crate::async_runtime::PromiseLayout;
use crate::gc::HandleId;
use crate::promise_reactions::{
  decode_waiters_ptr, enqueue_reaction_jobs, reverse_list, PromiseReactionNode, PromiseReactionVTable,
};
use crate::threading;

use super::{gc as async_gc, global as async_global, Task};

/// Promises that currently have pending reactions stored in their header.
///
/// This enables `rt_async_cancel_all` to abandon pending async work safely by dropping reaction
/// nodes that would otherwise never be enqueued (because the promise never settles after the host
/// shuts down timers/I/O).
static PROMISES_WITH_PENDING_REACTIONS: Lazy<GcAwareMutex<HashSet<usize>>> =
  Lazy::new(|| GcAwareMutex::new(HashSet::new()));

pub(crate) fn track_pending_reactions(promise: *mut PromiseHeader) {
  if promise.is_null() {
    return;
  }
  PROMISES_WITH_PENDING_REACTIONS
    .lock()
    // Track by raw promise header pointer. Promises are currently allocated in the non-moving bump
    // arena; if promises become GC-movable in the future, this tracking mechanism must be updated to
    // use stable handles/roots.
    .insert(promise as usize);
}

pub(crate) fn untrack_pending_reactions(promise: *mut PromiseHeader) {
  if promise.is_null() {
    return;
  }
  PROMISES_WITH_PENDING_REACTIONS.lock().remove(&(promise as usize));
}
/// Internal promise state used while a promise is being settled.
///
/// These values are not part of the public ABI; external code should only observe
/// `PromiseHeader::{PENDING,FULFILLED,REJECTED}`.
const STATE_FULFILLING: u8 = 3;
const STATE_REJECTING: u8 = 4;

/// Promise header flag indicating the promise has an associated out-of-line payload buffer.
///
/// Currently this is used by `rt_parallel_spawn_promise` promises: the worker writes its result into
/// the payload returned by `rt_promise_payload_ptr` and then settles the promise via
/// `rt_promise_fulfill` / `rt_promise_reject`.
///
/// This flag is stored in [`PromiseHeader::flags`] (see [`PROMISE_FLAG_HAS_PAYLOAD`]).

/// Raw sentinel value stored in `value_root`/`error_root` to represent `None`.
///
/// `HandleId` is a packed `{ index: u32, generation: u32 }`. The underlying handle table starts
/// generations at 1 so `HandleId(0)` can be used as a stable sentinel.
const ROOT_HANDLE_NONE: u64 = 0;

#[inline]
fn encode_root_handle(h: Option<HandleId>) -> u64 {
  h.map(|h| h.to_u64()).unwrap_or(ROOT_HANDLE_NONE)
}

#[inline]
fn decode_root_handle(raw: u64) -> Option<HandleId> {
  if raw == ROOT_HANDLE_NONE {
    None
  } else {
    Some(HandleId::from_u64(raw))
  }
}

#[repr(C)]
pub struct RtPromise {
  /// ABI-stable header prefix.
  pub header: PromiseHeader,
  /// Fulfillment value (valid when `header.state == FULFILLED`).
  value: AtomicUsize,
  /// Rejection reason (valid when `header.state == REJECTED`).
  error: AtomicUsize,
  /// Fulfillment value root handle (valid when `header.state == FULFILLED`).
  value_root: AtomicU64,
  /// Rejection reason root handle (valid when `header.state == REJECTED`).
  error_root: AtomicU64,
}

impl RtPromise {
  fn new_pending() -> Self {
    Self {
      header: PromiseHeader {
        // Legacy promises are not allocated via `rt_alloc` today; keep their GC header inert.
        obj: crate::gc::ObjHeader {
          type_desc: core::ptr::null(),
          meta: AtomicUsize::new(0),
        },
        state: core::sync::atomic::AtomicU8::new(PromiseHeader::PENDING),
        waiters: core::sync::atomic::AtomicUsize::new(0),
        flags: core::sync::atomic::AtomicU8::new(0),
      },
      value: AtomicUsize::new(0),
      error: AtomicUsize::new(0),
      value_root: AtomicU64::new(ROOT_HANDLE_NONE),
      error_root: AtomicU64::new(ROOT_HANDLE_NONE),
    }
  }
}

impl Drop for RtPromise {
  fn drop(&mut self) {
    let value_raw = self.value_root.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
    if let Some(h) = decode_root_handle(value_raw) {
      let _ = crate::roots::global_persistent_handle_table().free(h);
    }
    let error_raw = self.error_root.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
    if let Some(h) = decode_root_handle(error_raw) {
      let _ = crate::roots::global_persistent_handle_table().free(h);
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
    PromiseHeader::FULFILLED => {
      let h = unsafe { &*ptr }.value_root.load(Ordering::Acquire);
      if let Some(h) = decode_root_handle(h) {
        let value = crate::roots::global_persistent_handle_table()
          .get(h)
          .unwrap_or_else(|| std::process::abort());
        PromiseOutcome::Fulfilled(value.cast())
      } else {
        PromiseOutcome::Fulfilled(unsafe { &*ptr }.value.load(Ordering::Acquire) as ValueRef)
      }
    }
    PromiseHeader::REJECTED => {
      let h = unsafe { &*ptr }.error_root.load(Ordering::Acquire);
      if let Some(h) = decode_root_handle(h) {
        let err = crate::roots::global_persistent_handle_table()
          .get(h)
          .unwrap_or_else(|| std::process::abort());
        PromiseOutcome::Rejected(err.cast())
      } else {
        PromiseOutcome::Rejected(unsafe { &*ptr }.error.load(Ordering::Acquire) as ValueRef)
      }
    }
    // Includes `PENDING` + internal settling states.
    _ => PromiseOutcome::Pending,
  }
}

pub(crate) fn promise_new() -> PromiseRef {
  PromiseRef(Box::into_raw(Box::new(RtPromise::new_pending())) as *mut runtime_native_abi::PromiseHeader)
}

pub(crate) fn promise_new_with_payload(layout: PromiseLayout) -> PromiseRef {
  let payload = if layout.size == 0 {
    core::ptr::null_mut()
  } else {
    let align = layout.align.max(1);
    if !align.is_power_of_two() {
      crate::trap::rt_trap_invalid_arg("promise payload align must be a power of two");
    }
    crate::alloc::alloc_bytes(layout.size, align, "promise payload")
  };

  let promise = Box::new(RtPromise::new_pending());
  // Store the payload pointer even while pending; this allows worker threads to obtain the buffer
  // without settling the promise.
  promise.value.store(payload as usize, Ordering::Relaxed);
  promise
    .header
    .flags
    // Publish the payload pointer before setting the "has payload" flag so that a thread reading
    // `flags` with `Acquire` will also observe the `value` store.
    .store(PROMISE_FLAG_HAS_PAYLOAD, Ordering::Release);
  PromiseRef(Box::into_raw(promise) as *mut runtime_native_abi::PromiseHeader)
}

pub(crate) fn promise_payload_ptr(p: PromiseRef) -> *mut u8 {
  if p.is_null() {
    return core::ptr::null_mut();
  }

  // `rt_promise_payload_ptr` is part of the stable C ABI and accepts a generic `PromiseRef` (which
  // may refer to a native async-ABI promise that is only a `PromiseHeader` prefix). Avoid casting to
  // `RtPromise` unless we know this handle refers to one of our payload promises.
  let header = promise_header_ref(p);
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }

  let flags = unsafe { &(*header).flags }.load(Ordering::Acquire);
  if flags & PROMISE_FLAG_HAS_PAYLOAD == 0 {
    return core::ptr::null_mut();
  }
  // Safety: `PROMISE_FLAG_HAS_PAYLOAD` is only set by `promise_new_with_payload`, which allocates
  // an `RtPromise` (header prefix + out-of-line payload pointer stored in `value`).
  let ptr = promise_ptr(p);
  unsafe { &(*ptr).value }.load(Ordering::Acquire) as *mut u8
}

/// Attempt to fulfill a payload promise created by `rt_parallel_spawn_promise`.
///
/// Unlike [`promise_resolve`], this does **not** overwrite the promise's `value` field: for payload
/// promises `value` stores the out-of-line payload pointer returned by `rt_promise_payload_ptr`.
pub(crate) fn promise_try_fulfill_payload(p: PromiseRef) -> bool {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return false;
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
    // Best-effort: if this was an external-pending payload promise, ensure the count is not left
    // stuck in the presence of duplicate settles.
    maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
    return false;
  }

  // Preserve `value` (payload pointer). Clear any stale rejection reason.
  unsafe { &(*ptr).error }.store(0, Ordering::Relaxed);
  state.store(PromiseHeader::FULFILLED, Ordering::Release);

  drain_reactions(ptr);
  maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
  true
}

pub(crate) fn promise_fulfill_payload(p: PromiseRef) {
  let _ = promise_try_fulfill_payload(p);
}

/// Attempt to reject a payload promise created by `rt_parallel_spawn_promise`.
///
/// The promise's payload buffer is still accessible via `rt_promise_payload_ptr`. For legacy
/// awaiters, we report the payload pointer as the rejection reason (`await_error`).
pub(crate) fn promise_try_reject_payload(p: PromiseRef) -> bool {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return false;
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
    maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
    return false;
  }

  let payload = unsafe { &(*ptr).value }.load(Ordering::Acquire);
  unsafe { &(*ptr).error }.store(payload, Ordering::Relaxed);
  state.store(PromiseHeader::REJECTED, Ordering::Release);

  // If no one attached a handler yet, schedule unhandled-rejection tracking.
  if !promise_is_handled(p) {
    crate::unhandled_rejection::on_reject(p);
  }

  drain_reactions(ptr);
  maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
  true
}

pub(crate) fn promise_reject_payload(p: PromiseRef) {
  let _ = promise_try_reject_payload(p);
}

#[repr(C)]
struct CallbackReaction {
  node: PromiseReactionNode,
  callback: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: Option<extern "C" fn(*mut u8)>,
  /// Optional GC root for `data` when it points at a GC-managed object that may relocate.
  gc_root: Option<async_gc::Root>,
}

extern "C" fn callback_reaction_run(node: *mut PromiseReactionNode, _promise: crate::async_abi::PromiseRef) {
  // Safety: allocated by `alloc_callback_reaction`.
  let node = unsafe { &*(node as *mut CallbackReaction) };
  let data = node.gc_root.as_ref().map(|r| r.ptr()).unwrap_or(node.data);
  crate::ffi::invoke_cb1(node.callback, data);
}

extern "C" fn callback_reaction_drop(node: *mut PromiseReactionNode) {
  // Safety: allocated by `alloc_callback_reaction`.
  unsafe {
    let node = Box::from_raw(node as *mut CallbackReaction);
    if let Some(drop_data) = node.drop_data {
      crate::ffi::invoke_cb1(drop_data, node.data);
    }
  }
}

static CALLBACK_REACTION_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
  run: callback_reaction_run,
  drop: callback_reaction_drop,
};

fn alloc_callback_reaction(
  callback: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: Option<extern "C" fn(*mut u8)>,
  gc_root: Option<async_gc::Root>,
) -> *mut PromiseReactionNode {
  let node = Box::new(CallbackReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &CALLBACK_REACTION_VTABLE,
    },
    callback,
    data,
    drop_data,
    gc_root,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

fn push_reaction(ptr: *mut RtPromise, node: *mut PromiseReactionNode) {
  let reactions = unsafe { &(*ptr).header.waiters };
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

fn drain_reactions(ptr: *mut RtPromise) {
  let promise = ptr.cast::<PromiseHeader>();
  let reactions = unsafe { &(*ptr).header.waiters };
  let head_val = reactions.swap(0, Ordering::AcqRel);
  let mut head = decode_waiters_ptr(head_val);
  if head.is_null() {
    // No more reactions; ensure we don't retain the promise in the tracking set.
    untrack_pending_reactions(promise);
    return;
  }

  // The promise no longer owns any pending reactions, so it can be removed from the tracking set
  // even before we schedule the drained list.
  untrack_pending_reactions(promise);

  // The list is pushed in LIFO order; reverse to preserve FIFO registration order.
  head = unsafe { reverse_list(head) };
  enqueue_reaction_jobs(promise, head);
}

#[inline]
fn maybe_clear_external_pending(promise: *mut PromiseHeader) {
  if promise.is_null() {
    return;
  }
  if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  let prev = unsafe { &(*promise).flags }.fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel);
  if (prev & PROMISE_FLAG_EXTERNAL_PENDING) != 0 {
    crate::async_rt::external_pending_dec();
  }
}

pub(crate) fn promise_is_handled(p: PromiseRef) -> bool {
  if p.is_null() {
    // Null is a "never settles" sentinel and is not eligible for rejection tracking.
    return true;
  }

  // IMPORTANT: do not cast to `RtPromise` here.
  //
  // `PromiseRef` is an ABI handle whose concrete allocation layout depends on the promise producer:
  // - `async_rt::promise` allocates `RtPromise` (which embeds `PromiseHeader` + additional fields),
  // - native async ABI promises are arbitrary `PromiseHeader + payload` allocations created by
  //   generated code.
  //
  // The rejection tracker only needs the `PromiseHeader` prefix; dereferencing `RtPromise` would be
  // UB for smaller native-ABI promise layouts.
  let header = promise_header_ref(p);
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  unsafe { &*header }.is_handled()
}

pub(crate) fn promise_mark_handled(p: PromiseRef) {
  crate::unhandled_rejection::mark_handled(p);
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
      crate::ffi::abort_on_callback_panic(|| unsafe {
        let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
        drop_fn(node);
      });
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

  // Mark "handled" as soon as someone attaches a reaction (await/then).
  promise_mark_handled(p);

  push_reaction(ptr, node);
  track_pending_reactions(ptr.cast::<PromiseHeader>());

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
  let node = alloc_callback_reaction(on_settle, data, None, None);
  promise_register_reaction(p, node);
}

/// Like [`promise_then`], but treats `data` as a pointer to a GC-managed object that must remain
/// alive (and relocatable) while the callback is queued.
///
/// # Safety
/// `data` must be a valid pointer to a GC-managed object base pointer for the eventual moving GC.
pub(crate) fn promise_then_rooted(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  if p.is_null() {
    // Treat null as "never settles": keep it pending without retaining `data`.
    return;
  }
  let gc_root = if data.is_null() {
    None
  } else {
    // Safety: caller promises `data` is a GC-managed object pointer.
    Some(unsafe { async_gc::Root::new_unchecked(data) })
  };
  let node = alloc_callback_reaction(on_settle, data, None, gc_root);
  promise_register_reaction(p, node);
}

/// Like [`promise_then_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot) so a moving GC can update it if lock acquisition blocks while registering the
/// persistent root.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
pub(crate) unsafe fn promise_then_rooted_h(
  p: PromiseRef,
  on_settle: extern "C" fn(*mut u8),
  slot: crate::roots::GcHandle,
) {
  if p.is_null() {
    // Treat null as "never settles": keep it pending without retaining `slot`.
    return;
  }

  // Safety: caller promises `slot` points at a GC-managed object base pointer.
  let gc_root = Some(unsafe { async_gc::Root::new_from_slot_unchecked(slot) });

  // Avoid holding a raw GC pointer across any potentially blocking operations: the callback will
  // reload from `gc_root` at execution time.
  let node = alloc_callback_reaction(on_settle, null_mut(), None, gc_root);
  promise_register_reaction(p, node);
}

pub(crate) fn promise_then_with_drop(
  p: PromiseRef,
  on_settle: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
) {
  if p.is_null() {
    // Treat null as "never settles": immediately discard owned callback state.
    crate::ffi::invoke_cb1(drop_data, data);
    return;
  }
  let node = alloc_callback_reaction(on_settle, data, Some(drop_data), None);
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
    maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
    if let Some(hook) = hook {
      hook.notify_resolved();
    }
    return;
  }

  // Defensive: remove any previously installed persistent roots before storing the new value.
  let value_raw = unsafe { &(*ptr).value_root }.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
  if let Some(h) = decode_root_handle(value_raw) {
    let _ = crate::roots::global_persistent_handle_table().free(h);
  }
  let error_raw = unsafe { &(*ptr).error_root }.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
  if let Some(h) = decode_root_handle(error_raw) {
    let _ = crate::roots::global_persistent_handle_table().free(h);
  }

  let root = (!value.is_null()).then(|| crate::roots::global_persistent_handle_table().alloc(value.cast()));

  // Publish the result before flipping to the externally-visible fulfilled state.
  unsafe { &(*ptr).value }.store(value as usize, Ordering::Relaxed);
  unsafe { &(*ptr).error }.store(0, Ordering::Relaxed);
  unsafe { &(*ptr).value_root }.store(encode_root_handle(root), Ordering::Relaxed);
  unsafe { &(*ptr).error_root }.store(ROOT_HANDLE_NONE, Ordering::Relaxed);
  state.store(PromiseHeader::FULFILLED, Ordering::Release);

  drain_reactions(ptr);
  maybe_clear_external_pending(ptr.cast::<PromiseHeader>());

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
    maybe_clear_external_pending(ptr.cast::<PromiseHeader>());
    if let Some(hook) = hook {
      hook.notify_resolved();
    }
    return;
  }

  // Defensive: remove any previously installed persistent roots before storing the new error.
  let value_raw = unsafe { &(*ptr).value_root }.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
  if let Some(h) = decode_root_handle(value_raw) {
    let _ = crate::roots::global_persistent_handle_table().free(h);
  }
  let error_raw = unsafe { &(*ptr).error_root }.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
  if let Some(h) = decode_root_handle(error_raw) {
    let _ = crate::roots::global_persistent_handle_table().free(h);
  }

  let root = (!err.is_null()).then(|| crate::roots::global_persistent_handle_table().alloc(err.cast()));

  unsafe { &(*ptr).error }.store(err as usize, Ordering::Relaxed);
  unsafe { &(*ptr).value }.store(0, Ordering::Relaxed);
  unsafe { &(*ptr).error_root }.store(encode_root_handle(root), Ordering::Relaxed);
  unsafe { &(*ptr).value_root }.store(ROOT_HANDLE_NONE, Ordering::Relaxed);
  state.store(PromiseHeader::REJECTED, Ordering::Release);

  // If no one attached a handler yet, schedule unhandled-rejection tracking.
  if !promise_is_handled(p) {
    crate::unhandled_rejection::on_reject(p);
  }

  drain_reactions(ptr);
  maybe_clear_external_pending(ptr.cast::<PromiseHeader>());

  if let Some(hook) = hook {
    hook.notify_resolved();
  }
}

/// Drop all pending promise reactions without running them.
///
/// This is used by `rt_async_cancel_all` to ensure awaiting coroutines (and other `then` callbacks)
/// are properly torn down if the host stops driving the event loop before those promises settle.
pub(crate) fn cancel_all_pending_reactions() {
  let promises: Vec<*mut PromiseHeader> = {
    let mut set = PROMISES_WITH_PENDING_REACTIONS.lock();
    set.drain().map(|addr| addr as *mut PromiseHeader).collect()
  };

  for promise in promises {
    if promise.is_null() {
      continue;
    }
    if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
      std::process::abort();
    }

    let reactions = unsafe { &(*promise).waiters };
    let head_val = reactions.swap(0, Ordering::AcqRel);
    let mut head = decode_waiters_ptr(head_val);
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
        let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
        drop_fn(head);
      });

      head = next;
    }

    // If this promise was counted as "external pending" (e.g. a parallel task), clear the flag so
    // a later settlement can't perturb `EXTERNAL_PENDING` after teardown.
    maybe_clear_external_pending(promise);
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

/// Test-only hook: execute `f` while holding the pending-reactions tracking set lock.
///
/// This exists so integration tests can deterministically force contention on the
/// `PROMISES_WITH_PENDING_REACTIONS` lock while exercising stop-the-world coordination.
#[doc(hidden)]
pub(crate) fn debug_with_pending_reactions_lock<R>(f: impl FnOnce() -> R) -> R {
  let _guard = PROMISES_WITH_PENDING_REACTIONS.lock();
  f()
}

/// Test-only helper: destroy a legacy `RtPromise` allocated by this module.
///
/// # Safety
/// `p` must be a promise handle previously returned by [`promise_new`] or
/// [`promise_new_with_payload`]. Passing a non-`RtPromise` allocation (e.g. a native async-ABI
/// `PromiseHeader + payload`) is UB.
#[doc(hidden)]
pub(crate) unsafe fn debug_drop_promise(p: PromiseRef) {
  promise_drop(p);
}

pub(crate) struct PromiseWaiterRaceHook {
  stage: AtomicU8,
}

impl PromiseWaiterRaceHook {
  pub(crate) fn new() -> Self {
    Self {
      stage: AtomicU8::new(0),
    }
  }

  fn notify_waiter_checked_pending(&self) {
    self.stage.store(1, Ordering::Release);
  }

  fn wait_for_resolved(&self) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while self.stage.load(Ordering::Acquire) < 2 {
      let now = std::time::Instant::now();
      if now >= deadline {
        panic!("timed out waiting for promise to be resolved during race hook");
      }
      // Keep this as a pure spin/yield loop instead of blocking on a condvar/mutex: we want tests
      // to remain stop-the-world-safe even under contention (blocked threads do not poll
      // safepoints).
      threading::safepoint_poll();
      std::thread::yield_now();
    }
  }

  fn wait_for_waiter_checked_pending(&self) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while self.stage.load(Ordering::Acquire) < 1 {
      let now = std::time::Instant::now();
      if now >= deadline {
        panic!("timed out waiting for waiter registration during race hook");
      }
      threading::safepoint_poll();
      std::thread::yield_now();
    }
  }

  fn notify_resolved(&self) {
    self.stage.store(2, Ordering::Release);
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
  unsafe { &(*ptr).header.waiters }.load(Ordering::Acquire) == 0
}

// -----------------------------------------------------------------------------
// Promise resolution procedure (PromiseResolve / thenable assimilation)
// -----------------------------------------------------------------------------

static SELF_RESOLUTION_TYPE_ERROR: u8 = 0;

#[inline]
fn self_resolution_error() -> ValueRef {
  (&SELF_RESOLUTION_TYPE_ERROR as *const u8).cast_mut().cast()
}

fn promise_is_pending(p: PromiseRef) -> bool {
  matches!(promise_outcome(p), PromiseOutcome::Pending)
}

pub(crate) fn promise_resolve_into(dst: PromiseRef, input: PromiseResolveInput) {
  // Fast path: ignore if already settled.
  if !promise_is_pending(dst) {
    return;
  }

  match input.kind {
    PromiseResolveKind::Value => {
      let value = unsafe { input.payload.value };
      promise_resolve(dst, value);
    }
    PromiseResolveKind::Promise => {
      let src = unsafe { input.payload.promise };
      promise_resolve_promise(dst, src);
    }
    PromiseResolveKind::Thenable => {
      let thenable = unsafe { input.payload.thenable };
      promise_resolve_thenable(dst, thenable);
    }
  }
}

pub(crate) fn promise_resolve_promise(dst: PromiseRef, src: PromiseRef) {
  if dst.is_null() {
    return;
  }
  if !promise_is_pending(dst) {
    return;
  }

  if src == dst {
    promise_reject(dst, self_resolution_error());
    return;
  }
  if src.is_null() {
    // Null promises are treated as "never settles" sentinels.
    return;
  }

  struct AdoptContinuation {
    dst_root: async_gc::Root,
    src_root: async_gc::Root,
  }

  extern "C" fn adopt_on_settle(data: *mut u8) {
    // Safety: allocated by `promise_resolve_promise` and freed by the callback reaction drop hook.
    let cont = unsafe { &*(data as *const AdoptContinuation) };
    let dst = PromiseRef(cont.dst_root.ptr().cast());
    let src = PromiseRef(cont.src_root.ptr().cast());
    match promise_outcome(src) {
      PromiseOutcome::Fulfilled(v) => promise_resolve(dst, v),
      PromiseOutcome::Rejected(e) => promise_reject(dst, e),
      PromiseOutcome::Pending => {
        // Shouldn't happen (callback only runs after settlement) but be robust: resubscribe.
        let cont = Box::new(AdoptContinuation {
          dst_root: cont.dst_root.clone(),
          src_root: cont.src_root.clone(),
        });
        promise_then_with_drop(src, adopt_on_settle, Box::into_raw(cont) as *mut u8, drop_adopt_continuation);
      }
    }
  }

  extern "C" fn drop_adopt_continuation(data: *mut u8) {
    // Safety: allocated by `Box::into_raw` in `promise_resolve_promise`.
    unsafe {
      drop(Box::from_raw(data as *mut AdoptContinuation));
    }
  }

  let cont = Box::new(AdoptContinuation {
    dst_root: unsafe { async_gc::Root::new_unchecked(dst.0.cast::<u8>()) },
    src_root: unsafe { async_gc::Root::new_unchecked(src.0.cast::<u8>()) },
  });
  promise_then_with_drop(src, adopt_on_settle, Box::into_raw(cont) as *mut u8, drop_adopt_continuation);
}

pub(crate) fn promise_resolve_thenable(dst: PromiseRef, thenable: ThenableRef) {
  if dst.is_null() {
    return;
  }
  if !promise_is_pending(dst) {
    return;
  }

  // Self-resolution check (thenable refers to the same object as the promise).
  if thenable.ptr == dst.0.cast::<u8>() {
    promise_reject(dst, self_resolution_error());
    return;
  }

  #[repr(C)]
  struct ThenableJob {
    dst_root: async_gc::Root,
    thenable: ThenableRef,
    /// Optional root for the thenable pointer so it stays alive and relocatable while the
    /// `PromiseResolveThenableJob` microtask is queued.
    thenable_root: Option<async_gc::Root>,
  }

  struct ThenableResolver {
    dst_handle: AtomicU64,
    called: AtomicBool,
  }

  extern "C" fn thenable_resolve(data: *mut u8, value: PromiseResolveInput) {
    // Safety: resolver is intentionally leaked until process exit (promises are currently leaked as
    // well).
    let resolver = unsafe { &*(data as *const ThenableResolver) };
    if resolver
      .called
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      return;
    }

    let handle_raw = resolver.dst_handle.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
    if let Some(id) = decode_root_handle(handle_raw) {
      let dst = PromiseRef(
        crate::roots::global_persistent_handle_table()
          .get(id)
          .unwrap_or(core::ptr::null_mut())
          .cast(),
      );
      promise_resolve_into(dst, value);
      let _ = crate::roots::global_persistent_handle_table().free(id);
    }
  }

  extern "C" fn thenable_reject(data: *mut u8, reason: ValueRef) {
    // Safety: resolver is intentionally leaked until process exit (promises are currently leaked as
    // well).
    let resolver = unsafe { &*(data as *const ThenableResolver) };
    if resolver
      .called
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      return;
    }

    let handle_raw = resolver.dst_handle.swap(ROOT_HANDLE_NONE, Ordering::AcqRel);
    if let Some(id) = decode_root_handle(handle_raw) {
      let dst = PromiseRef(
        crate::roots::global_persistent_handle_table()
          .get(id)
          .unwrap_or(core::ptr::null_mut())
          .cast(),
      );
      promise_reject(dst, reason);
      let _ = crate::roots::global_persistent_handle_table().free(id);
    }
  }

  extern "C" fn run_thenable_job(data: *mut u8) {
    // Safety: allocated by `promise_resolve_thenable` and freed by the task drop hook.
    let job = unsafe { &*(data as *const ThenableJob) };
    let dst = PromiseRef(job.dst_root.ptr().cast());

    // If the destination promise was already settled by another path, do not invoke the thenable at
    // all.
    if !promise_is_pending(dst) {
      return;
    }

    if job.thenable.vtable.is_null() {
      promise_reject(dst, self_resolution_error());
      return;
    }

    let dst_handle = crate::roots::global_persistent_handle_table().alloc(dst.0.cast());
    let resolver = Box::new(ThenableResolver {
      dst_handle: AtomicU64::new(encode_root_handle(Some(dst_handle))),
      called: AtomicBool::new(false),
    });
    let resolver_ptr = Box::into_raw(resolver) as *mut u8;

    let call_then = unsafe { (*job.thenable.vtable).call_then };

    let thenable_ptr = job
      .thenable_root
      .as_ref()
      .map(|r| r.ptr())
      .unwrap_or(job.thenable.ptr);

    let thrown = unsafe {
      crate::ffi::invoke_thenable_call(call_then, thenable_ptr, thenable_resolve, thenable_reject, resolver_ptr)
    };

    if !thrown.is_null() {
      thenable_reject(resolver_ptr, thrown);
    }
  }

  let thenable_root = (!thenable.ptr.is_null())
    // Safety: `thenable.ptr` must be a valid pointer to a GC-managed object base pointer if it is
    // intended to be relocatable.
    .then(|| unsafe { async_gc::Root::new_unchecked(thenable.ptr) });
  let dst_root = unsafe { async_gc::Root::new_unchecked(dst.0.cast::<u8>()) };
  let job = Box::new(ThenableJob { dst_root, thenable, thenable_root });

  extern "C" fn drop_thenable_job(data: *mut u8) {
    // Safety: allocated by `Box::into_raw` above.
    unsafe {
      drop(Box::from_raw(data as *mut ThenableJob));
    }
  }

  async_global().enqueue_microtask(Task::new_with_drop(
    run_thenable_job,
    Box::into_raw(job) as *mut u8,
    drop_thenable_job,
  ));
}

/// Drop a legacy runtime-native promise.
///
/// Promises are currently leaked in production builds; this exists so tests and
/// embedders can deterministically release promises and any persistent GC roots
/// they keep alive.
pub(crate) fn promise_drop(p: PromiseRef) {
  let ptr = promise_ptr(p);
  if ptr.is_null() {
    return;
  }

  // Dropping a promise must not leak pending reaction nodes or leave stale pointers in the global
  // tracking set used by `rt_async_cancel_all`.
  //
  // Note: this is primarily used by tests/embedders; production builds currently leak promises.
  // Still, make the drop path robust so that:
  // - pending reactions are destroyed without running, and
  // - future cancellation/teardown does not observe a freed promise pointer.
  untrack_pending_reactions(ptr.cast::<PromiseHeader>());

  // Drop any pending reaction nodes stored on the promise header.
  //
  // These would otherwise be leaked if the promise never settles (or if the embedding drops the
  // promise early). We treat promise drop as a teardown operation, so callbacks are *not* executed.
  let reactions = unsafe { &(*ptr).header.waiters };
  let head_val = reactions.swap(0, Ordering::AcqRel);
  let mut head = decode_waiters_ptr(head_val);
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
      let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) = std::mem::transmute((&*vtable).drop);
      drop_fn(head);
    });
    head = next;
  }

  // If this promise was keeping the event loop non-idle via an external pending count (e.g. a
  // parallel task), clear the flag and decrement that count.
  maybe_clear_external_pending(ptr.cast::<PromiseHeader>());

  // Also ensure the unhandled-rejection tracker doesn't retain a freed promise pointer.
  crate::unhandled_rejection::forget_promise(p);

  // SAFETY: `PromiseRef` values are created from `Box::into_raw` in `promise_new`.
  unsafe { drop(Box::from_raw(ptr)) };
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn promises_with_pending_reactions_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    const TIMEOUT: Duration = Duration::from_secs(2);

    std::thread::scope(|scope| {
      // Thread A holds the pending-reactions set lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to track a promise while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = PROMISES_WITH_PENDING_REACTIONS.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the pending-reactions lock");

      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();

        // Create a promise in this thread so we don't have to send raw pointers across threads.
        let p = promise_new();
        // `track_pending_reactions` tracks the `PromiseHeader` prefix (not the legacy `RtPromise`
        // wrapper type).
        let ptr = promise_header_ref(p);

        c_start_rx.recv().unwrap();

        track_pending_reactions(ptr);
        // Drop cleans up the tracking set entry too.
        promise_drop(p);

        c_done_tx.send(()).unwrap();
        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the lock before starting STW.
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
      let start = Instant::now();
      loop {
        let mut native_safe = false;
        threading::registry::for_each_thread(|t| {
          if t.id() == c_id {
            native_safe = t.is_native_safe();
          }
        });

        if native_safe {
          break;
        }
        if start.elapsed() > TIMEOUT {
          panic!("thread C did not enter a GC-safe region while blocked on the pending-reactions lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      struct ResumeOnDrop;
      impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
          crate::threading::safepoint::rt_gc_resume_world();
        }
      }
      let _resume = ResumeOnDrop;

      // Let thread A release the lock and reach the safepoint.
      a_release_tx.send(()).unwrap();

      assert!(
        crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
        "world failed to stop within timeout; pending-reactions lock contention must not block STW"
      );

      // Resume the world so the contending `track_pending_reactions` can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("promise tracking should complete after world is resumed");
    });
  }
}
