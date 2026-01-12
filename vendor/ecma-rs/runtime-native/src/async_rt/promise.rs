use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::abi::{LegacyPromiseRef, PromiseRef, PromiseResolveInput, PromiseResolveKind, ThenableRef, ValueRef};
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING, PROMISE_FLAG_HAS_PAYLOAD};
use crate::gc::HandleId;
use crate::promise_reactions::{
  decode_waiters_ptr, enqueue_reaction_jobs, reverse_list, PromiseReactionNode, PromiseReactionVTable,
};
use crate::sync::GcAwareMutex;
use crate::threading;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Weak;

use super::{gc as async_gc, global as async_global, Task};

/// Internal flag bit in [`PromiseHeader::flags`] indicating this promise handle refers to a legacy
/// [`RtPromise`] allocation owned by this module.
///
/// This is required because `PromiseRef` is an ABI-level opaque handle that may refer to:
/// - legacy boxed `RtPromise` values, or
/// - native async-ABI promises (arbitrary `PromiseHeader`-prefixed objects), including payload
///   promises returned by `rt_parallel_spawn_promise`.
///
/// The runtime must never cast a `PromiseRef` to `RtPromise` unless it can prove the underlying
/// allocation is actually an `RtPromise`; otherwise dereferencing would be UB for smaller promise
/// layouts.
const PROMISE_FLAG_LEGACY_RT_PROMISE: u8 = 1 << 3;

#[derive(Clone)]
enum PendingPromiseKeepAlive {
  /// Promise memory is assumed to remain valid while tracked (e.g. bump-allocated native promises,
  /// legacy `RtPromise`s).
  None,
  /// Promise is backed by reference-counted Rust ownership (e.g. `promise_api::Promise<T>`).
  ///
  /// Store a `Weak` so tracking does **not** keep the promise alive: dropping the last `Arc` should
  /// still free the promise and drop its reaction nodes normally.
  ///
  /// During `rt_async_cancel_all`, we attempt to upgrade the weak reference. If the promise is
  /// already dropped, the upgrade fails and we skip dereferencing the raw pointer (avoids
  /// use-after-free when cancellation races with `Drop` on another thread).
  Weak(Weak<dyn Any + Send + Sync>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PendingPromiseKey {
  /// Promise memory is not GC-managed (and therefore not relocatable).
  ///
  /// We track these by raw pointer so legacy boxed promises don't allocate extra persistent handles
  /// just for cancellation bookkeeping.
  Raw(usize),
  /// Promise is GC-managed and may be relocated; track via a stable persistent handle id.
  Rooted(HandleId),
}

#[inline]
fn promise_is_gc_managed(promise: *mut PromiseHeader) -> bool {
  if promise.is_null() {
    return false;
  }
  // Safety: `PromiseHeader` begins with a GC `ObjHeader` prefix at offset 0.
  // Non-GC-managed promises (legacy `RtPromise`, etc.) keep the header inert by setting
  // `ObjHeader::type_desc` to null.
  unsafe { !(*promise).obj.type_desc.is_null() }
}

/// Promises that currently have pending reactions stored in their header.
///
/// This enables `rt_async_cancel_all` to abandon pending async work safely by dropping reaction
/// nodes that would otherwise never be enqueued (because the promise never settles after the host
/// shuts down timers/I/O).
static PROMISES_WITH_PENDING_REACTIONS: Lazy<
  GcAwareMutex<HashMap<PendingPromiseKey, PendingPromiseKeepAlive>>,
> = Lazy::new(|| GcAwareMutex::new(HashMap::new()));

pub(crate) fn track_pending_reactions(promise: *mut PromiseHeader) {
  track_pending_reactions_keepalive(promise, PendingPromiseKeepAlive::None);
}

pub(crate) fn track_pending_reactions_weak(
  promise: *mut PromiseHeader,
  keepalive: Weak<dyn Any + Send + Sync>,
) {
  track_pending_reactions_keepalive(promise, PendingPromiseKeepAlive::Weak(keepalive));
}

fn track_pending_reactions_keepalive(
  promise: *mut PromiseHeader,
  keepalive: PendingPromiseKeepAlive,
) {
  if promise.is_null() {
    return;
  }
  if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }

  // Non-GC-managed promises are not relocatable: track by raw pointer to avoid allocating an extra
  // persistent handle-table entry (some rooted scheduling API tests assert the handle count stays
  // constant while promises are pending).
  if !promise_is_gc_managed(promise) {
    let key = PendingPromiseKey::Raw(promise as usize);
    let mut map = PROMISES_WITH_PENDING_REACTIONS.lock();
    if let Some(existing) = map.get_mut(&key) {
      // Preserve any existing keepalive (it may have been upgraded to `Weak` by another caller).
      if matches!(existing, PendingPromiseKeepAlive::None)
        && matches!(keepalive, PendingPromiseKeepAlive::Weak(_))
      {
        *existing = keepalive;
      }
    } else {
      map.insert(key, keepalive);
    }
    return;
  }

  // Promises can be GC-movable; track them via stable persistent handle IDs (and keep them alive)
  // instead of raw pointers, which become stale after relocation.
  //
  // Important: contended `GcAwareMutex::lock()` acquisition may enter a GC-safe ("NativeSafe")
  // region while blocked. While a thread is NativeSafe, a moving GC will not relocate raw GC
  // pointers in its registers/locals. Therefore, we must not block on the pending-reactions lock
  // while holding an unrooted raw `promise` pointer.
  //
  // Strategy:
  // - Fast path: attempt a non-blocking `try_lock`. If it succeeds, we did not block and can use
  //   the raw pointer for lookup.
  // - Slow path (contended): first materialize a stable persistent handle ID, then drop any
  //   temporary handle-stack roots and only then block on the mutex so the thread can transition
  //   into a GC-safe region while waiting.
  //
  // The optional `keepalive` policy determines whether it is safe to dereference this pointer
  // during teardown if cancellation races with the promise being dropped on another thread.
  let table = crate::roots::global_persistent_handle_table();

  // Addressable slot holding the promise pointer. Root it via the per-thread handle stack while we
  // do any potentially blocking work (e.g. handle-table lock acquisition).
  let mut promise_ptr: *mut u8 = promise.cast::<u8>();
  let mut scope = crate::roots::RootScope::new();
  scope.push(&mut promise_ptr as *mut *mut u8);

  // Uncontended fast path: acquire the lock without blocking.
  if let Some(mut map) = PROMISES_WITH_PENDING_REACTIONS.try_lock() {
    let mut ids_to_free: Vec<HandleId> = Vec::new();

    let mut found: Option<HandleId> = None;
    for key in map.keys().copied() {
      let PendingPromiseKey::Rooted(id) = key else {
        continue;
      };
      match table.get(id) {
        Some(p) if p == promise_ptr => {
          if found.is_none() {
            found = Some(id);
          } else {
            // Defensive: remove duplicates so we don't leak persistent handles if tracking raced.
            ids_to_free.push(id);
          }
        }
        None => {
          // Stale handle (freed elsewhere); remove it so it doesn't bloat the map.
          ids_to_free.push(id);
        }
        _ => {}
      }
    }

    for id in &ids_to_free {
      map.remove(&PendingPromiseKey::Rooted(*id));
    }

    match found {
      Some(id) => {
        // Preserve any existing keepalive (it may have been upgraded to `Weak` by another caller).
        if let Some(existing) = map.get_mut(&PendingPromiseKey::Rooted(id)) {
          if matches!(existing, PendingPromiseKeepAlive::None)
            && matches!(keepalive, PendingPromiseKeepAlive::Weak(_))
          {
            *existing = keepalive;
          }
        }
      }
      None => {
        // Safety: `promise_ptr` is a valid pointer slot rooted via `RootScope`.
        let id = unsafe { table.alloc_from_slot(&mut promise_ptr as *mut *mut u8) };
        map.insert(PendingPromiseKey::Rooted(id), keepalive);
      }
    }

    drop(map);
    drop(scope);

    for id in ids_to_free {
      let _ = table.free(id);
    }
    return;
  }

  // Contended slow path: create a stable handle ID *before* blocking on the mutex.
  //
  // Safety: `promise_ptr` is a valid pointer slot rooted via `RootScope`.
  let id_new = unsafe { table.alloc_from_slot(&mut promise_ptr as *mut *mut u8) };
  // Drop the handle-stack root before blocking so `GcAwareMutex` can enter a GC-safe region.
  drop(scope);

  let mut ids_to_free: Vec<HandleId> = Vec::new();
  {
    let mut map = PROMISES_WITH_PENDING_REACTIONS.lock();

    // Root the comparison pointer so if a GC runs while contending on handle-table locks (inside
    // `table.get`), the slot is updated.
    let mut want_ptr = table.get(id_new).unwrap_or(null_mut());
    let mut scope = crate::roots::RootScope::new();
    scope.push(&mut want_ptr as *mut *mut u8);

    let mut found: Option<HandleId> = None;
    for key in map.keys().copied() {
      let PendingPromiseKey::Rooted(id) = key else {
        continue;
      };
      match table.get(id) {
        Some(p) if p == want_ptr => {
          if found.is_none() {
            found = Some(id);
          } else {
            // Defensive: remove duplicates so we don't leak persistent handles if tracking raced.
            ids_to_free.push(id);
          }
        }
        None => {
          // Stale handle (freed elsewhere); remove it so it doesn't bloat the map.
          ids_to_free.push(id);
        }
        _ => {}
      }
    }

    for id in &ids_to_free {
      map.remove(&PendingPromiseKey::Rooted(*id));
    }

    match found {
      Some(id) => {
        // Preserve any existing keepalive (it may have been upgraded to `Weak` by another caller).
        if let Some(existing) = map.get_mut(&PendingPromiseKey::Rooted(id)) {
          if matches!(existing, PendingPromiseKeepAlive::None)
            && matches!(keepalive, PendingPromiseKeepAlive::Weak(_))
          {
            *existing = keepalive;
          }
        }
        // We already had a handle for this promise; free the one we just allocated.
        ids_to_free.push(id_new);
      }
      None => {
        map.insert(PendingPromiseKey::Rooted(id_new), keepalive);
      }
    }
  }

  for id in ids_to_free {
    let _ = table.free(id);
  }
}

pub(crate) fn untrack_pending_reactions(promise: *mut PromiseHeader) {
  if promise.is_null() {
    return;
  }
  if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }

  if !promise_is_gc_managed(promise) {
    let mut map = PROMISES_WITH_PENDING_REACTIONS.lock();
    map.remove(&PendingPromiseKey::Raw(promise as usize));
    return;
  }
  let table = crate::roots::global_persistent_handle_table();

  // See `track_pending_reactions_keepalive` for the reasoning behind this structure:
  // avoid blocking on a GC-aware mutex while holding handle-stack roots.
  let mut promise_ptr: *mut u8 = promise.cast::<u8>();
  let mut scope = crate::roots::RootScope::new();
  scope.push(&mut promise_ptr as *mut *mut u8);

  // Fast path: avoid blocking if the lock is uncontended.
  if let Some(mut map) = PROMISES_WITH_PENDING_REACTIONS.try_lock() {
    let mut to_remove = Vec::new();

    for key in map.keys().copied() {
      let PendingPromiseKey::Rooted(id) = key else {
        continue;
      };
      match table.get(id) {
        Some(p) if p == promise_ptr => {
          // Defensive: remove all matches to avoid leaking duplicate roots if tracking was raced.
          to_remove.push(id);
        }
        None => {
          // Stale handle (freed elsewhere); remove it so it doesn't bloat the set.
          to_remove.push(id);
        }
        _ => {}
      }
    }

    for id in &to_remove {
      map.remove(&PendingPromiseKey::Rooted(*id));
    }

    drop(map);
    drop(scope);

    for id in to_remove {
      let _ = table.free(id);
    }
    return;
  }

  // Contended slow path: create a stable handle ID for `promise` before blocking on the map lock,
  // then drop the handle-stack root so the thread can enter a GC-safe region while blocked.
  //
  // Safety: `promise_ptr` is an addressable slot rooted via `RootScope`.
  let target_id = unsafe { table.alloc_from_slot(&mut promise_ptr as *mut *mut u8) };
  drop(scope);

  let ids_to_free: Vec<HandleId> = {
    let mut map = PROMISES_WITH_PENDING_REACTIONS.lock();

    let mut want_ptr = table.get(target_id).unwrap_or(null_mut());
    let mut scope = crate::roots::RootScope::new();
    scope.push(&mut want_ptr as *mut *mut u8);

    let mut to_remove = Vec::new();
    for key in map.keys().copied() {
      let PendingPromiseKey::Rooted(id) = key else {
        continue;
      };
      match table.get(id) {
        Some(p) if p == want_ptr => {
          to_remove.push(id);
        }
        None => {
          to_remove.push(id);
        }
        _ => {}
      }
    }

    for id in &to_remove {
      map.remove(&PendingPromiseKey::Rooted(*id));
    }

    // Free the temporary handle we created for stable lookup.
    to_remove.push(target_id);
    to_remove
  };

  for id in ids_to_free {
    let _ = table.free(id);
  }
}
/// Internal promise state used while a promise is being settled.
///
/// These values are not part of the public ABI; external code should only observe
/// `PromiseHeader::{PENDING,FULFILLED,REJECTED}`.
const STATE_FULFILLING: u8 = 3;
const STATE_REJECTING: u8 = 4;

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
        flags: core::sync::atomic::AtomicU8::new(PROMISE_FLAG_LEGACY_RT_PROMISE),
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
fn promise_header_ref(p: PromiseRef) -> crate::async_abi::PromiseRef {
  // `PromiseRef` is an opaque handle over the ABI. Every runtime-native promise allocation begins
  // with a `PromiseHeader` prefix at offset 0.
  p.0.cast::<PromiseHeader>()
}

pub(crate) enum PromiseOutcome {
  Pending,
  Fulfilled(ValueRef),
  Rejected(ValueRef),
}

/// Layout for "payload promises" created by `rt_parallel_spawn_promise`.
///
/// The only stable contract is:
/// - `PromiseHeader` is at offset 0, and
/// - the payload pointer is stored immediately after the header.
///
/// The promise's actual payload buffer is allocated out-of-line and is accessible via
/// `rt_promise_payload_ptr`.
#[repr(C)]
struct PayloadPromise {
  header: PromiseHeader,
  payload_ptr: AtomicUsize,
}

enum PromiseClass {
  /// Null promises are treated as a "never settles" sentinel.
  Null,
  /// Payload promise (identified via `PROMISE_FLAG_HAS_PAYLOAD`).
  Payload(*mut PayloadPromise),
  /// Legacy value promise owned by this module (identified via `PROMISE_FLAG_LEGACY_RT_PROMISE`).
  LegacyValue(*mut RtPromise),
  /// Some other `PromiseHeader`-prefixed promise layout (native async ABI, etc).
  Unknown(*mut PromiseHeader),
}

#[inline]
fn promise_header_ptr(p: PromiseRef) -> *mut PromiseHeader {
  if p.is_null() {
    return null_mut();
  }
  let header = p.0.cast::<PromiseHeader>();
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  header
}

#[inline]
fn classify_promise(p: PromiseRef) -> PromiseClass {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return PromiseClass::Null;
  }

  // Safety: `header` is non-null and properly aligned.
  let flags = unsafe { &(*header).flags }.load(Ordering::Acquire);

  if (flags & PROMISE_FLAG_HAS_PAYLOAD) != 0 {
    let payload = header.cast::<PayloadPromise>();
    if (payload as usize) % core::mem::align_of::<PayloadPromise>() != 0 {
      std::process::abort();
    }
    return PromiseClass::Payload(payload);
  }

  if (flags & PROMISE_FLAG_LEGACY_RT_PROMISE) != 0 {
    let legacy = header.cast::<RtPromise>();
    if (legacy as usize) % core::mem::align_of::<RtPromise>() != 0 {
      std::process::abort();
    }
    return PromiseClass::LegacyValue(legacy);
  }

  PromiseClass::Unknown(header)
}

#[inline]
fn rt_promise_ptr(p: PromiseRef) -> *mut RtPromise {
  let header = promise_header_ptr(p);
  if header.is_null() {
    return null_mut();
  }
  let flags = unsafe { &(*header).flags }.load(Ordering::Acquire);
  if (flags & PROMISE_FLAG_LEGACY_RT_PROMISE) == 0 {
    return null_mut();
  }
  let ptr = header.cast::<RtPromise>();
  if (ptr as usize) % core::mem::align_of::<RtPromise>() != 0 {
    std::process::abort();
  }
  ptr
}

pub(crate) fn promise_outcome(p: PromiseRef) -> PromiseOutcome {
  match classify_promise(p) {
    PromiseClass::Null => PromiseOutcome::Pending,
    PromiseClass::Payload(payload) => {
      let state = unsafe { &(*payload).header.state }.load(Ordering::Acquire);
      match state {
        PromiseHeader::FULFILLED => {
          let payload = unsafe { &(*payload).payload_ptr }.load(Ordering::Acquire) as ValueRef;
          PromiseOutcome::Fulfilled(payload)
        }
        PromiseHeader::REJECTED => {
          let payload = unsafe { &(*payload).payload_ptr }.load(Ordering::Acquire) as ValueRef;
          PromiseOutcome::Rejected(payload)
        }
        _ => PromiseOutcome::Pending,
      }
    }
    PromiseClass::LegacyValue(ptr) => {
      let state = unsafe { &(*ptr).header.state }.load(Ordering::Acquire);
      match state {
        PromiseHeader::FULFILLED => {
          let h = unsafe { &(*ptr).value_root }.load(Ordering::Acquire);
          if let Some(h) = decode_root_handle(h) {
            let value = crate::roots::global_persistent_handle_table()
              .get(h)
              .unwrap_or_else(|| std::process::abort());
            PromiseOutcome::Fulfilled(value.cast())
          } else {
            PromiseOutcome::Fulfilled(unsafe { &(*ptr).value }.load(Ordering::Acquire) as ValueRef)
          }
        }
        PromiseHeader::REJECTED => {
          let h = unsafe { &(*ptr).error_root }.load(Ordering::Acquire);
          if let Some(h) = decode_root_handle(h) {
            let err = crate::roots::global_persistent_handle_table()
              .get(h)
              .unwrap_or_else(|| std::process::abort());
            PromiseOutcome::Rejected(err.cast())
          } else {
            PromiseOutcome::Rejected(unsafe { &(*ptr).error }.load(Ordering::Acquire) as ValueRef)
          }
        }
        _ => PromiseOutcome::Pending,
      }
    }
    PromiseClass::Unknown(header) => {
      let state = unsafe { &(*header).state }.load(Ordering::Acquire);
      match state {
        PromiseHeader::FULFILLED => PromiseOutcome::Fulfilled(core::ptr::null_mut()),
        PromiseHeader::REJECTED => PromiseOutcome::Rejected(core::ptr::null_mut()),
        _ => PromiseOutcome::Pending,
      }
    }
  }
}

pub(crate) fn promise_new() -> PromiseRef {
  PromiseRef(
    Box::into_raw(Box::new(RtPromise::new_pending())) as *mut runtime_native_abi::PromiseHeader
  )
}

pub(crate) fn promise_payload_ptr(p: PromiseRef) -> *mut u8 {
  match classify_promise(p) {
    PromiseClass::Payload(payload) => {
      unsafe { &(*payload).payload_ptr }.load(Ordering::Acquire) as *mut u8
    }
    PromiseClass::Unknown(header) if promise_is_gc_managed(header) => {
      // Native async-ABI promises are GC-managed objects whose payload begins immediately after the
      // `PromiseHeader` prefix at offset 0. Expose a pointer to that inline payload so callers can
      // write/read structured results that may contain GC pointers.
      //
      // The returned pointer points into the GC heap and is only valid until the next GC/safepoint;
      // callers must not store it across safepoints without also keeping the base `PromiseRef`
      // (object base pointer) alive as a GC root.
      let base = unsafe {
        let obj = &(*header).obj;
        if obj.is_forwarded() {
          obj.forwarding_ptr()
        } else {
          header.cast::<u8>()
        }
      };
      unsafe { base.add(core::mem::size_of::<PromiseHeader>()) }
    }
    _ => core::ptr::null_mut(),
  }
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

extern "C" fn callback_reaction_run(
  node: *mut PromiseReactionNode,
  _promise: crate::async_abi::PromiseRef,
) {
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

fn drain_reactions(ptr: *mut RtPromise) {
  let promise = ptr.cast::<PromiseHeader>();
  drain_reactions_generic(promise);
}

fn drain_reactions_generic(promise: *mut PromiseHeader) {
  let reactions = unsafe { &(*promise).waiters };
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
  let prev =
    unsafe { &(*promise).flags }.fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel);
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
  let header = promise_header_ptr(p);
  if header.is_null() {
    // Treat null as "never settles": discard the node so it doesn't leak.
    if !node.is_null() {
      let vtable = unsafe { (*node).vtable };
      if vtable.is_null() {
        std::process::abort();
      }
      crate::ffi::abort_on_callback_panic(|| unsafe {
        let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) =
          std::mem::transmute((&*vtable).drop);
        drop_fn(node);
      });
    }
    return;
  }

  // Test-only deterministic race hook: allow a resolver thread to settle/drain while this
  // registration is paused before linking into `reactions`.
  if let Some(hook) = debug_waiter_race_hook() {
    // The legacy race hook is only wired into the legacy value-promise settle path
    // (`promise_resolve`/`promise_reject`). Avoid waiting on promises that won't ever notify it.
    let flags = unsafe { &(*header).flags }.load(Ordering::Acquire);
    let is_legacy_value =
      (flags & PROMISE_FLAG_LEGACY_RT_PROMISE) != 0 && (flags & PROMISE_FLAG_HAS_PAYLOAD) == 0;
    if is_legacy_value {
      let state = unsafe { &(*header).state }.load(Ordering::Acquire);
      if state == PromiseHeader::PENDING {
        hook.notify_waiter_checked_pending();
        hook.wait_for_resolved();
      }
    }
  }

  // Mark "handled" as soon as someone attaches a reaction (await/then).
  promise_mark_handled(p);

  push_reaction(header, node);
  track_pending_reactions(header);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*header).state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions_generic(header);
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
  let PromiseClass::LegacyValue(ptr) = classify_promise(p) else {
    return;
  };

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

  let root = (!value.is_null())
    .then(|| crate::roots::global_persistent_handle_table().alloc_movable(value.cast()));

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
  let PromiseClass::LegacyValue(ptr) = classify_promise(p) else {
    return;
  };

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

  let root =
    (!err.is_null()).then(|| crate::roots::global_persistent_handle_table().alloc_movable(err.cast()));

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
  let promises: Vec<(PendingPromiseKey, PendingPromiseKeepAlive)> = {
    let mut map = PROMISES_WITH_PENDING_REACTIONS.lock();
    map.drain().collect()
  };

  let table = crate::roots::global_persistent_handle_table();

  for (key, keepalive) in promises {
    let (needs_keepalive, _keepalive) = match keepalive {
      PendingPromiseKeepAlive::None => (false, None),
      PendingPromiseKeepAlive::Weak(w) => (true, w.upgrade()),
    };
    if needs_keepalive && _keepalive.is_none() {
      // Promise was dropped concurrently with cancellation; its `Drop` implementation is
      // responsible for discarding any waiter list. Avoid dereferencing the pointer.
      if let PendingPromiseKey::Rooted(id) = key {
        let _ = table.free(id);
      }
      continue;
    }

    let (promise, id_to_free) = match key {
      PendingPromiseKey::Raw(ptr) => (ptr as *mut PromiseHeader, None),
      PendingPromiseKey::Rooted(id) => (
        table
          .get(id)
          .unwrap_or(core::ptr::null_mut())
          .cast::<PromiseHeader>(),
        Some(id),
      ),
    };
    if !promise.is_null() {
      if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
        std::process::abort();
      }

      // If this promise was counted as "external pending" (e.g. a parallel task), clear the flag so
      // a later settlement can't perturb `EXTERNAL_PENDING` after teardown.
      //
      // Do this *before* dropping reaction nodes so we don't touch `promise` after potentially freeing
      // it (e.g. if some reaction node holds the last strong reference to the promise allocation).
      maybe_clear_external_pending(promise);

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
          let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) =
            std::mem::transmute((&*vtable).drop);
          drop_fn(head);
        });

        head = next;
      }
    }

    // Free the persistent root so we no longer keep this promise alive (if it is GC-managed).
    if let Some(id) = id_to_free {
      let _ = table.free(id);
    }
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
/// `p` must be a valid [`PromiseRef`] handle.
///
/// If `p` does not refer to a legacy `RtPromise` allocation (e.g. it is a native async-ABI promise
/// or a payload promise), this is a no-op.
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

static DEBUG_WAITER_RACE_HOOK: AtomicPtr<PromiseWaiterRaceHook> =
  AtomicPtr::new(core::ptr::null_mut());

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
  let header = promise_header_ptr(p);
  if header.is_null() {
    return true;
  }
  unsafe { &(*header).waiters }.load(Ordering::Acquire) == 0
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
      let src: LegacyPromiseRef = unsafe { input.payload.promise };
      // `LegacyPromiseRef` is an ABI-level opaque promise pointer. By contract it points at a
      // `PromiseHeader` prefix at offset 0, but the concrete promise layout may vary (legacy
      // `RtPromise`, GC-managed payload promise, native async ABI promise, ...).
      //
      // Convert explicitly so we don't accidentally assume the underlying layout is `RtPromise`.
      promise_resolve_promise(dst, PromiseRef(src.cast()));
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
        promise_then_with_drop(
          src,
          adopt_on_settle,
          Box::into_raw(cont) as *mut u8,
          drop_adopt_continuation,
        );
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
  promise_then_with_drop(
    src,
    adopt_on_settle,
    Box::into_raw(cont) as *mut u8,
    drop_adopt_continuation,
  );
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

    let dst_handle = crate::roots::global_persistent_handle_table().alloc_movable(dst.0.cast());
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
      crate::ffi::invoke_thenable_call(
        call_then,
        thenable_ptr,
        thenable_resolve,
        thenable_reject,
        resolver_ptr,
      )
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
  let job = Box::new(ThenableJob {
    dst_root,
    thenable,
    thenable_root,
  });

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

/// Drop a legacy runtime-native promise (`RtPromise`).
///
/// Promises are currently leaked in production builds; this exists so tests and
/// embedders can deterministically release promises and any persistent GC roots
/// they keep alive.
///
/// If `p` does not refer to a legacy `RtPromise` allocation (e.g. it is a native async-ABI promise
/// or a non-legacy payload promise), this is a no-op.
pub(crate) fn promise_drop(p: PromiseRef) {
  let ptr = rt_promise_ptr(p);
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
  let header = ptr.cast::<PromiseHeader>();
  untrack_pending_reactions(header);

  // Drop any pending reaction nodes stored on the promise header.
  //
  // These would otherwise be leaked if the promise never settles (or if the embedding drops the
  // promise early). We treat promise drop as a teardown operation, so callbacks are *not* executed.
  let reactions = unsafe { &(*header).waiters };
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
      let drop_fn: extern "C-unwind" fn(*mut PromiseReactionNode) =
        std::mem::transmute((&*vtable).drop);
      drop_fn(head);
    });
    head = next;
  }

  // If this promise was keeping the event loop non-idle via an external pending count (e.g. a
  // parallel task), clear the flag and decrement that count.
  maybe_clear_external_pending(header);

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

  #[test]
  fn promises_with_pending_reactions_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    // Stop-the-world handshakes can take much longer in debug builds (especially
    // under parallel test execution on multi-agent hosts). Keep release builds
    // strict, but give debug builds enough slack to avoid flaky timeouts.
    const TIMEOUT: Duration = if cfg!(debug_assertions) {
      Duration::from_secs(30)
    } else {
      Duration::from_secs(2)
    };

    std::thread::scope(|scope| {
      // Thread A holds the pending-reactions set lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to track a promise while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_attempt_tx, c_attempt_rx) = mpsc::channel::<()>();
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

        // `RtPromise` embeds `PromiseHeader` at offset 0.
        c_attempt_tx.send(()).unwrap();
        track_pending_reactions(ptr);
        // Drop cleans up the tracking set entry too.
        promise_drop(p);

        c_done_tx.send(()).unwrap();
        threading::unregister_current_thread();
      });

      c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the lock before starting STW.
      c_start_tx.send(()).unwrap();
      c_attempt_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should attempt promise tracking while the lock is held");

      // Note: when contending while holding handle-stack roots, `GcAwareMutex` intentionally avoids
      // entering `NativeSafe` and instead busy-spins with safepoint polls. The deadlock-free property
      // is validated by the stop-the-world barrier below.

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
