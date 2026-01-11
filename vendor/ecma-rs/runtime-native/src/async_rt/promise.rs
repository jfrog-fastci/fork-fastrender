use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::abi::{PromiseRef, PromiseResolveInput, PromiseResolveKind, ThenableRef, ValueRef};
use crate::async_abi::PromiseHeader;
use crate::async_runtime::PromiseLayout;
use crate::gc::HandleId;
use crate::promise_reactions::{enqueue_reaction_jobs, reverse_list, PromiseReactionNode, PromiseReactionVTable};
use crate::threading;

use super::{global as async_global, Task};

use std::panic::{catch_unwind, AssertUnwindSafe};

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
const FLAG_HAS_PAYLOAD: u8 = 1 << 1;

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
          .unwrap_or(core::ptr::null_mut());
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
          .unwrap_or(core::ptr::null_mut());
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
  PromiseRef(Box::into_raw(Box::new(RtPromise::new_pending())) as *mut core::ffi::c_void)
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
  // Store the payload pointer in `value` even while pending; this allows worker threads to obtain
  // the buffer without settling the promise.
  promise.value.store(payload as usize, Ordering::Relaxed);
  promise
    .header
    .flags
    // Publish the payload pointer before setting the "has payload" flag so that a thread reading
    // `flags` with `Acquire` will also observe the `value` store.
    .store(FLAG_HAS_PAYLOAD, Ordering::Release);
  PromiseRef(Box::into_raw(promise) as *mut core::ffi::c_void)
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
  if flags & FLAG_HAS_PAYLOAD == 0 {
    return core::ptr::null_mut();
  }

  // Safety: `FLAG_HAS_PAYLOAD` is only set by `promise_new_with_payload`, which allocates an
  // `RtPromise` (header prefix + out-of-line payload pointer stored in `value`).
  let ptr = promise_ptr(p);
  unsafe { &(*ptr).value }.load(Ordering::Acquire) as *mut u8
}

#[repr(C)]
struct CallbackReaction {
  node: PromiseReactionNode,
  callback: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: Option<extern "C" fn(*mut u8)>,
}

extern "C" fn callback_reaction_run(node: *mut PromiseReactionNode, _promise: crate::async_abi::PromiseRef) {
  // Safety: allocated by `alloc_callback_reaction`.
  let node = unsafe { &*(node as *mut CallbackReaction) };
  (node.callback)(node.data);
}

extern "C" fn callback_reaction_drop(node: *mut PromiseReactionNode) {
  // Safety: allocated by `alloc_callback_reaction`.
  unsafe {
    let node = Box::from_raw(node as *mut CallbackReaction);
    if let Some(drop_data) = node.drop_data {
      drop_data(node.data);
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
) -> *mut PromiseReactionNode {
  let node = Box::new(CallbackReaction {
    node: PromiseReactionNode {
      next: null_mut(),
      vtable: &CALLBACK_REACTION_VTABLE,
    },
    callback,
    data,
    drop_data,
  });
  Box::into_raw(node) as *mut PromiseReactionNode
}

fn push_reaction(ptr: *mut RtPromise, node: *mut PromiseReactionNode) {
  let reactions = unsafe { &(*ptr).header.waiters };
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
  let reactions = unsafe { &(*ptr).header.waiters };
  let mut head = reactions.swap(0, Ordering::AcqRel) as *mut PromiseReactionNode;
  if head.is_null() {
    return;
  }

  // The list is pushed in LIFO order; reverse to preserve FIFO registration order.
  head = unsafe { reverse_list(head) };
  let promise = ptr.cast::<PromiseHeader>();
  enqueue_reaction_jobs(promise, head);
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

  // Mark "handled" as soon as someone attaches a reaction (await/then).
  promise_mark_handled(p);

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
  let node = alloc_callback_reaction(on_settle, data, None);
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
    drop_data(data);
    return;
  }
  let node = alloc_callback_reaction(on_settle, data, Some(drop_data));
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
static THENABLE_CALL_PANICKED: u8 = 0;

#[inline]
fn self_resolution_error() -> ValueRef {
  (&SELF_RESOLUTION_TYPE_ERROR as *const u8).cast_mut().cast()
}

#[inline]
fn thenable_panic_error() -> ValueRef {
  (&THENABLE_CALL_PANICKED as *const u8).cast_mut().cast()
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
    dst: PromiseRef,
    src: PromiseRef,
  }

  extern "C" fn adopt_on_settle(data: *mut u8) {
    // Safety: allocated by `promise_resolve_promise` and freed by the callback reaction drop hook.
    let cont = unsafe { &*(data as *const AdoptContinuation) };
    match promise_outcome(cont.src) {
      PromiseOutcome::Fulfilled(v) => promise_resolve(cont.dst, v),
      PromiseOutcome::Rejected(e) => promise_reject(cont.dst, e),
      PromiseOutcome::Pending => {
        // Shouldn't happen (callback only runs after settlement) but be robust: resubscribe.
        let cont = Box::new(AdoptContinuation {
          dst: cont.dst,
          src: cont.src,
        });
        promise_then_with_drop(cont.src, adopt_on_settle, Box::into_raw(cont) as *mut u8, drop_adopt_continuation);
      }
    }
  }

  extern "C" fn drop_adopt_continuation(data: *mut u8) {
    // Safety: allocated by `Box::into_raw` in `promise_resolve_promise`.
    unsafe {
      drop(Box::from_raw(data as *mut AdoptContinuation));
    }
  }

  let cont = Box::new(AdoptContinuation { dst, src });
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
  if thenable.ptr.cast::<core::ffi::c_void>() == dst.0 {
    promise_reject(dst, self_resolution_error());
    return;
  }

  #[repr(C)]
  struct ThenableJob {
    dst: PromiseRef,
    thenable: ThenableRef,
  }

  struct ThenableResolver {
    dst: PromiseRef,
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

    promise_resolve_into(resolver.dst, value);
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

    promise_reject(resolver.dst, reason);
  }

  extern "C" fn run_thenable_job(data: *mut u8) {
    // Safety: allocated by `promise_resolve_thenable` and freed by the task drop hook.
    let job = unsafe { &*(data as *const ThenableJob) };

    // If the destination promise was already settled by another path, do not invoke the thenable at
    // all.
    if !promise_is_pending(job.dst) {
      return;
    }

    if job.thenable.vtable.is_null() {
      promise_reject(job.dst, self_resolution_error());
      return;
    }

    let resolver = Box::new(ThenableResolver {
      dst: job.dst,
      called: AtomicBool::new(false),
    });
    let resolver_ptr = Box::into_raw(resolver) as *mut u8;

    let call_then = unsafe { (*job.thenable.vtable).call_then };

    let thrown = catch_unwind(AssertUnwindSafe(|| unsafe {
      (call_then)(job.thenable.ptr, thenable_resolve, thenable_reject, resolver_ptr)
    }))
    .unwrap_or_else(|_| thenable_panic_error());

    if !thrown.is_null() {
      thenable_reject(resolver_ptr, thrown);
    }
  }

  let job = Box::new(ThenableJob { dst, thenable });

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

  // SAFETY: `PromiseRef` values are created from `Box::into_raw` in `promise_new`.
  unsafe {
    drop(Box::from_raw(ptr));
  }
}
