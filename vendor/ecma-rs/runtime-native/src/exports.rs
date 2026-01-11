use crate::abi::PromiseRef;
use crate::abi::PromiseResolveInput;
use crate::abi::RtCoroutineHeader;
use crate::abi::RtShapeId;
use crate::abi::TaskId;
use crate::abi::TimerId;
use crate::abi::ThenableRef;
use crate::abi::ValueRef;
use crate::abi::IoWatcherId;
use crate::async_runtime::PromiseLayout;
use crate::alloc;
use crate::array;
use crate::array::RtArrayHeader;
use crate::async_runtime;
use crate::async_rt;
use crate::async_rt::WatcherId;
use crate::ffi::abort_on_panic;
use crate::async_abi::PromiseHeader;
use crate::gc::ObjHeader;
use crate::gc::TypeDescriptor;
use crate::gc::WeakHandle;
use crate::gc::YOUNG_SPACE;
use crate::BackingStoreAllocator;
use crate::shape_table;
use crate::threading;
use crate::threading::registry;
use crate::trap;
use crate::Runtime;
use crate::Thread;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[inline]
fn promise_is_pending(p: PromiseRef) -> bool {
  if p.is_null() {
    return true;
  }

  // `PromiseRef` is an opaque pointer in the stable ABI, but by contract it must point to a
  // `PromiseHeader` at offset 0 of the allocation.
  let header = p.0.cast::<PromiseHeader>();
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }

  // `PromiseHeader::state` stores both externally-visible states (PENDING/FULFILLED/REJECTED) and
  // internal transient settling states. Treat any non-(FULFILLED|REJECTED) state as pending.
  let state = unsafe { &(*header).state }.load(Ordering::Acquire);
  state != PromiseHeader::FULFILLED && state != PromiseHeader::REJECTED
}

// Promise flag used by legacy promises for unhandled-rejection tracking.
const PROMISE_FLAG_HANDLED: u8 = 1 << 0;

#[repr(C)]
struct BlockOnReaction {
  node: crate::promise_reactions::PromiseReactionNode,
}

extern "C" fn block_on_reaction_run(
  _node: *mut crate::promise_reactions::PromiseReactionNode,
  _promise: crate::async_abi::PromiseRef,
) {
}

extern "C" fn block_on_reaction_drop(node: *mut crate::promise_reactions::PromiseReactionNode) {
  if node.is_null() {
    return;
  }
  unsafe {
    drop(Box::from_raw(node as *mut BlockOnReaction));
  }
}

static BLOCK_ON_REACTION_VTABLE: crate::promise_reactions::PromiseReactionVTable =
  crate::promise_reactions::PromiseReactionVTable {
    run: block_on_reaction_run,
    drop: block_on_reaction_drop,
  };

#[inline]
fn alloc_block_on_reaction() -> *mut crate::promise_reactions::PromiseReactionNode {
  let node = Box::new(BlockOnReaction {
    node: crate::promise_reactions::PromiseReactionNode {
      next: core::ptr::null_mut(),
      vtable: &BLOCK_ON_REACTION_VTABLE,
    },
  });
  Box::into_raw(node) as *mut crate::promise_reactions::PromiseReactionNode
}

#[inline]
fn push_reaction(promise: *mut PromiseHeader, node: *mut crate::promise_reactions::PromiseReactionNode) {
  if promise.is_null() || node.is_null() {
    return;
  }

  let reactions = unsafe { &(*promise).reactions };
  loop {
    let head = reactions.load(Ordering::Acquire) as *mut crate::promise_reactions::PromiseReactionNode;
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
  if promise.is_null() {
    return;
  }

  let reactions = unsafe { &(*promise).reactions };
  let mut head = reactions.swap(0, Ordering::AcqRel) as *mut crate::promise_reactions::PromiseReactionNode;
  if head.is_null() {
    return;
  }

  // Preserve FIFO registration order.
  head = unsafe { crate::promise_reactions::reverse_list(head) };

  while !head.is_null() {
    let next = unsafe { (*head).next };
    unsafe {
      (*head).next = core::ptr::null_mut();
    }
    crate::promise_reactions::enqueue_reaction_job(promise, head);
    head = next;
  }
}

fn register_block_on_waker(p: PromiseRef) {
  if p.is_null() {
    // Null promises are treated as "never settles": nothing can wake.
    return;
  }

  let promise = p.0.cast::<PromiseHeader>();
  if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }

  // Mark "handled" to avoid reporting unhandled rejections while we are blocked waiting.
  let prev = unsafe { &(*promise).flags }.fetch_or(PROMISE_FLAG_HANDLED, Ordering::AcqRel);
  if (prev & PROMISE_FLAG_HANDLED) == 0 {
    crate::unhandled_rejection::on_handle(p);
  }

  let node = alloc_block_on_reaction();
  push_reaction(promise, node);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*promise).state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions(promise);
  }
}

#[inline(always)]
fn ensure_event_loop_thread_registered() {
  // The async runtime is driven by the main thread/event loop. Register it on
  // first use so GC can coordinate stop-the-world safepoints across all
  // mutator threads.
  crate::threading::register_current_thread(crate::threading::ThreadKind::Main);
}

fn thread_kind_from_abi(kind: u32) -> threading::ThreadKind {
  match kind {
    0 => threading::ThreadKind::Main,
    1 => threading::ThreadKind::Worker,
    2 => threading::ThreadKind::Io,
    3 => threading::ThreadKind::External,
    other => {
      // Avoid unwinding across the C ABI boundary.
      if cfg!(debug_assertions) {
        eprintln!("rt_thread_register: unknown thread kind {other} (expected 0..=3)");
        std::process::abort();
      }
      threading::ThreadKind::External
    }
  }
}

#[inline(always)]
fn mark_card_range(card_table: *mut AtomicU64, start_card: usize, end_card: usize) {
  debug_assert!(!card_table.is_null());
  debug_assert!(start_card <= end_card);

  let start_word = start_card / 64;
  let end_word = end_card / 64;
  let start_bit = start_card % 64;
  let end_bit = end_card % 64;

  unsafe {
    if start_word == end_word {
      let high_mask = if end_bit == 63 {
        !0u64
      } else {
        (1u64 << (end_bit + 1)) - 1
      };
      let low_mask = (!0u64) << start_bit;
      let mask = high_mask & low_mask;
      (*card_table.add(start_word)).fetch_or(mask, Ordering::Release);
      return;
    }

    // First word: mark from start_bit..=63.
    (*card_table.add(start_word)).fetch_or((!0u64) << start_bit, Ordering::Release);

    // Middle words: mark all bits.
    for word in (start_word + 1)..end_word {
      (*card_table.add(word)).fetch_or(!0u64, Ordering::Release);
    }

    // Last word: mark 0..=end_bit.
    let last_mask = if end_bit == 63 {
      !0u64
    } else {
      (1u64 << (end_bit + 1)) - 1
    };
    (*card_table.add(end_word)).fetch_or(last_mask, Ordering::Release);
  }
}

#[inline(always)]
fn ensure_current_thread_registered() {
  // Promise settlement and other runtime hooks may run on arbitrary threads
  // (I/O completions, external callbacks). Treat unregistered threads as
  // "external" by default so we don't incorrectly classify them as the main
  // event-loop thread.
  crate::threading::register_current_thread(crate::threading::ThreadKind::External);
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_alloc(size: usize, shape: RtShapeId) -> crate::roots::GcPtr {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc(size);

  // Don't let panics unwind across the extern "C" boundary.
  let res = catch_unwind(AssertUnwindSafe(|| {
    let desc = shape_table::lookup_rt_descriptor(shape);
    if size != desc.size as usize {
      crate::trap::rt_trap_invalid_arg("rt_alloc: size does not match registered shape descriptor");
    }

    let align = desc.align as usize;
    let obj = alloc::alloc_bytes(size, align, "rt_alloc");

    // Ensure pointer slots start out as null so tracing never sees uninitialized garbage.
    // SAFETY: `obj` is valid for `size` bytes.
    unsafe {
      std::ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = shape_table::lookup_type_descriptor(shape) as *const _;
      header.meta.store(0, Ordering::Relaxed);
    }

    obj
  }));

  match res {
    Ok(ptr) => ptr,
    Err(_) => std::process::abort(),
  }
}

/// Allocate a pinned (non-moving) object.
///
/// NOTE: The milestone runtime does not yet wire allocations into the GC. This entrypoint exists so
/// codegen/FFI can request a stable address today and so future GC-backed allocation can route
/// pinned objects to a non-moving space.
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_alloc_pinned(size: usize, shape: RtShapeId) -> crate::roots::GcPtr {
  // Don't let panics unwind across the extern "C" boundary.
  let res = catch_unwind(AssertUnwindSafe(|| {
    let desc = shape_table::lookup_rt_descriptor(shape);
    if size != desc.size as usize {
      crate::trap::rt_trap_invalid_arg(
        "rt_alloc_pinned: size does not match registered shape descriptor",
      );
    }

    let align = desc.align as usize;
    let obj = alloc::alloc_bytes(size, align, "rt_alloc_pinned");

    // SAFETY: `obj` is valid for `size` bytes.
    unsafe {
      std::ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = shape_table::lookup_type_descriptor(shape) as *const _;
      header.meta.store(0, Ordering::Relaxed);
      header.set_pinned(true);
    }

    obj
  }));

  match res {
    Ok(ptr) => ptr,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> crate::roots::GcPtr {
  let Some(spec) = array::decode_rt_array_elem_size(elem_size) else {
    crate::trap::rt_trap_invalid_arg("rt_alloc_array: invalid elem_size");
  };
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc_array(len, spec.elem_size);

  let size = array::checked_total_bytes(len, spec.elem_size)
    .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("rt_alloc_array: size overflow"));

  let obj = alloc::alloc_bytes_zeroed(size, 16, "rt_alloc_array");
  // SAFETY: `obj` points to `size` bytes of writable, zeroed memory.
  unsafe {
    let header = &mut *(obj as *mut ObjHeader);
    header.type_desc = &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor;
    header.meta.store(0, Ordering::Relaxed);

    let arr = &mut *(obj as *mut RtArrayHeader);
    arr.len = len;
    arr.elem_size = spec.elem_size as u32;
    arr.elem_flags = spec.elem_flags;
  }

  obj
}

#[no_mangle]
pub extern "C" fn rt_alloc_ptr_array(len: usize) -> *mut u8 {
  rt_alloc_array(len, array::RT_ARRAY_ELEM_PTR_FLAG | core::mem::size_of::<*mut u8>())
}

#[no_mangle]
pub extern "C" fn rt_array_len(obj: *mut u8) -> usize {
  if obj.is_null() {
    trap::rt_trap_invalid_arg("rt_array_len called with null");
  }
  // SAFETY: The ABI contract requires `obj` be a valid array object.
  unsafe {
    let mut obj = obj;
    let header = &*(obj as *const ObjHeader);
    if header.is_forwarded() {
      obj = header.forwarding_ptr();
    }
    let header = &*(obj as *const ObjHeader);
    if header.type_desc != &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
      trap::rt_trap_invalid_arg("rt_array_len called on non-array object");
    }
    (*(obj as *const RtArrayHeader)).len
  }
}

#[no_mangle]
pub extern "C" fn rt_array_data(obj: *mut u8) -> *mut u8 {
  if obj.is_null() {
    trap::rt_trap_invalid_arg("rt_array_data called with null");
  }
  // SAFETY: The ABI contract requires `obj` be a valid array object.
  unsafe {
    let mut obj = obj;
    let header = &*(obj as *const ObjHeader);
    if header.is_forwarded() {
      obj = header.forwarding_ptr();
    }
    let header = &*(obj as *const ObjHeader);
    if header.type_desc != &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
      trap::rt_trap_invalid_arg("rt_array_data called on non-array object");
    }
    obj.add(array::RT_ARRAY_DATA_OFFSET)
  }
}

/// Register the current OS thread with the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_init(kind: u32) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_init();

  threading::register_current_thread(thread_kind_from_abi(kind));
}

/// Unregister the current OS thread from the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_deinit() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_deinit();
  threading::unregister_current_thread();
}

/// Register the current OS thread with the runtime thread registry.
///
/// This is a stable compiler/runtime ABI entrypoint used by LLVM-generated code.
///
/// `kind` mapping:
/// - 0: Main
/// - 1: Worker
/// - 2: Io
/// - 3: External
#[no_mangle]
pub extern "C" fn rt_thread_register(kind: u32) -> u64 {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_init();

  threading::register_current_thread(thread_kind_from_abi(kind)).get()
}

/// Unregister the current OS thread from the runtime thread registry.
#[no_mangle]
pub extern "C" fn rt_thread_unregister() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_deinit();

  threading::unregister_current_thread();
}

/// Mark/unmark the current thread as parked (idle) inside the runtime.
///
/// When transitioning back to `parked == false` (unparking), this will perform a
/// safepoint poll before returning.
#[no_mangle]
pub extern "C" fn rt_thread_set_parked(parked: bool) {
  threading::set_parked(parked);
}

/// GC safepoint.
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_gc_safepoint() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_safepoint();

  // Fast path: if no stop-the-world is requested, `rt_gc_safepoint` is a cheap
  // no-op even for threads that are not registered with the runtime.
  if !crate::threading::safepoint::rt_gc_poll() {
    return;
  }

  // `rt_gc_safepoint` is only meaningful for threads that have been registered
  // with `rt_thread_init`. For non-attached threads this is a no-op: they do not
  // participate in stop-the-world coordination.
  if registry::current_thread_id().is_none() {
    return;
  }

  crate::threading::safepoint::rt_gc_safepoint();
}

/// Enter a GC safepoint and return the (possibly relocated) pointer stored in `slot`.
///
/// This is a `may_gc` helper intended for ABI design/testing. Any runtime function that may trigger
/// GC but also needs to use GC-managed pointer arguments must accept those pointers as
/// pointer-to-slot handles (see `docs/gc_handle_abi.md`).
///
/// # Safety
/// `slot` must be a valid writable pointer to a `*mut u8` slot.
#[no_mangle]
#[inline(never)]
pub unsafe extern "C" fn rt_gc_safepoint_relocate_h(
  slot: crate::roots::GcHandle,
) -> crate::roots::GcPtr {
  if slot.is_null() {
    crate::trap::rt_trap_invalid_arg("rt_gc_safepoint_relocate_h: slot was null");
  }
  rt_gc_safepoint();
  crate::roots::load_handle(slot)
}

/// Cheap GC poll used by compiler-inserted fast paths (e.g. loop backedge safepoints).
///
/// Returns `true` if a stop-the-world GC/safepoint is currently requested.
///
/// Generated code typically uses this in a "fast poll" sequence:
///
/// 1. `if rt_gc_poll() { rt_gc_safepoint(); }`
///
/// `native-js` marks this function as a GC leaf (`"gc-leaf-function"`) so
/// `rewrite-statepoints-for-gc` does not wrap the poll itself in a statepoint.
#[no_mangle]
pub extern "C" fn rt_gc_poll() -> bool {
  (crate::threading::safepoint::RT_GC_EPOCH.load(Ordering::Acquire) & 1) != 0
}

// LLVM `place-safepoints` poll function.
//
// LLVM's `place-safepoints` pass inserts calls to a symbol named
// `gc.safepoint_poll` in functions that use a statepoint-based GC strategy.
// Those calls are later rewritten into statepoints by `rewrite-statepoints-for-gc`.
//
// The runtime must provide the symbol so codegen can use `place-safepoints`
// without needing to synthesize its own poll function body in every module.
//
// NOTE: The actual `gc.safepoint_poll` symbol is implemented in per-architecture
// assembly (`arch/x86_64.rs`, `arch/aarch64.rs`). It must capture the *managed*
// caller's frame pointer and return address at the poll callsite so the GC can
// locate the correct stackmap record for root enumeration.

/// Update the active young-space address range used by the write barrier.
///
/// This must be called by the GC during initialization and after each nursery
/// flip/resize that changes the current young generation region.
#[no_mangle]
pub extern "C" fn rt_gc_set_young_range(start: *mut u8, end: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_set_young_range();
  YOUNG_SPACE.start.store(start as usize, Ordering::Release);
  YOUNG_SPACE.end.store(end as usize, Ordering::Release);
}

/// Debug/test helper: return the current young-space range.
///
/// # Safety
/// If `out_start`/`out_end` are non-null, they must be valid writable pointers.
#[no_mangle]
pub unsafe extern "C" fn rt_gc_get_young_range(
  out_start: *mut crate::roots::GcPtr,
  out_end: *mut crate::roots::GcPtr,
) {
  if !out_start.is_null() {
    *out_start = YOUNG_SPACE.start.load(Ordering::Acquire) as *mut u8;
  }
  if !out_end.is_null() {
    *out_end = YOUNG_SPACE.end.load(Ordering::Acquire) as *mut u8;
  }
}

// --- Process-global remembered set ---------------------------------------------------------------
//
// The exported write barrier (`rt_write_barrier`) is classified as `NoGC` and must not allocate or
// safepoint. We still need a process-global remembered-set for tests and for future minor GC wiring.
// To keep the barrier allocation-free, use a fixed-capacity array.
//
// If this overflows we abort: failing to record old→young edges is unsound for a generational GC.
const REMEMBERED_SET_CAPACITY: usize = 1 << 20; // ~1M entries = 8MB on 64-bit

struct FixedRememberedSet {
  len: AtomicUsize,
  entries: [AtomicUsize; REMEMBERED_SET_CAPACITY],
}

impl FixedRememberedSet {
  const fn new() -> Self {
    Self {
      len: AtomicUsize::new(0),
      entries: [const { AtomicUsize::new(0) }; REMEMBERED_SET_CAPACITY],
    }
  }

  #[inline]
  fn insert(&self, obj: *mut u8) {
    debug_assert!(!obj.is_null());
    let idx = self.len.fetch_add(1, Ordering::AcqRel);
    if idx >= REMEMBERED_SET_CAPACITY {
      // The write barrier must not allocate, so we cannot grow. Overflow would allow missing an
      // old→young edge, which can lead to use-after-move/free during minor GC.
      std::process::abort();
    }
    self.entries[idx].store(obj as usize, Ordering::Release);
  }

  fn clear(&self) {
    let len = self.len.swap(0, Ordering::AcqRel).min(REMEMBERED_SET_CAPACITY);
    for i in 0..len {
      // Only clear the runtime's remembered-set tracking (raw pointers). The per-object remembered
      // bit is owned by the objects themselves, and this helper is used primarily to avoid leaving
      // dangling raw pointers in tests.
      self.entries[i].store(0, Ordering::Release);
    }
  }
}

static REMEMBERED_SET: FixedRememberedSet = FixedRememberedSet::new();

/// Reset write barrier state for tests.
///
/// This clears only process-global state used by the exported barrier:
/// - the active nursery range (`YOUNG_SPACE`),
/// - and the runtime's remembered-set tracking (used by GC model tests).
///
/// Per-object metadata (e.g. the `REMEMBERED` header bit) is owned by the
/// objects themselves and is not cleared (the runtime cannot enumerate all
/// objects in the current milestone GC).
#[doc(hidden)]
pub fn clear_write_barrier_state_for_tests() {
  rt_gc_set_young_range(core::ptr::null_mut(), core::ptr::null_mut());
  REMEMBERED_SET.clear();
}

/// Debug/test helper: is the given object base pointer currently in the remembered set?
///
/// Currently this is equivalent to checking the `REMEMBERED` bit on the object's header.
#[doc(hidden)]
pub fn remembered_set_contains(obj: *mut u8) -> bool {
  if obj.is_null() {
    return false;
  }
  // Avoid UB: callers must pass an object base pointer.
  if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
    std::process::abort();
  }
  unsafe { (&*(obj as *const ObjHeader)).is_remembered() }
}

/// Debug/test helper: rebuild remembered-set tracking after a simulated minor GC.
///
/// Model-based generational GC tests (`runtime-native/tests/generational_model.rs`) simulate a
/// semispace nursery where young objects can survive multiple minor collections. The exported write
/// barrier sets the per-object `REMEMBERED` header bit and records newly-remembered objects in a
/// small process-global list ([`REMEMBERED_SET`]).
///
/// This helper performs a **sticky rebuild** by:
/// - retaining only objects for which `object_has_young_refs` returns `true`
/// - clearing the per-object `REMEMBERED` bit for removed objects
///
/// `objs` is the set of candidate old-generation objects that might be remembered.
#[doc(hidden)]
pub fn remembered_set_scan_and_rebuild_for_tests(
  objs: &[*mut u8],
  mut object_has_young_refs: impl FnMut(*mut u8) -> bool,
) {
  // Rebuild from the provided list of candidate old objects. `runtime-native` cannot currently
  // enumerate all objects from the heap, and the process-global remembered set may contain stale
  // pointers left behind by other tests.
  REMEMBERED_SET.clear();
  for &obj in objs {
    if obj.is_null() {
      continue;
    }
    if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
      std::process::abort();
    }
    // SAFETY: alignment checked above; `obj` is expected to be a valid object base pointer.
    let header = unsafe { &mut *(obj as *mut ObjHeader) };
    if object_has_young_refs(obj) {
      header.set_remembered(true);
      REMEMBERED_SET.insert(obj);
    } else {
      header.set_remembered(false);
    }
  }
}

#[inline]
unsafe fn remember_old_object(obj: *mut u8) {
  debug_assert!(!obj.is_null());
  // `rt_write_barrier` is classified as `NoGC` by the ABI contract (must not allocate or safepoint).
  // Record remembered objects into a fixed-capacity global set so tests (and future minor GC wiring)
  // can iterate remembered objects without scanning the entire heap.
  let header = &*(obj as *const ObjHeader);
  if header.set_remembered_idempotent() {
    REMEMBERED_SET.insert(obj);
  }
}
/// Write barrier for GC.
///
/// Records old→young pointer stores in the remembered set.
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier(obj: crate::roots::GcPtr, slot: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier();

  if obj.is_null() || slot.is_null() {
    return;
  }

  // Avoid UB on misaligned pointers: the barrier is specified to read a pointer-sized value from
  // `slot` and to treat `obj` as an `ObjHeader` base pointer.
  if (slot as usize) % std::mem::align_of::<*mut u8>() != 0 {
    std::process::abort();
  }
  if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
    std::process::abort();
  }

  // SAFETY: The write barrier contract requires `slot` be aligned and contain a
  // valid GC pointer or null.
  let value = (slot as *const *mut u8).read();
  if value.is_null() {
    return;
  }

  if !YOUNG_SPACE.contains(value as usize) {
    return;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find
  // the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  // Old → young store. Mark the base object as remembered.
  remember_old_object(obj);

  // If this object has a per-object card table, mark the card for the written slot.
  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if !card_table.is_null() {
    let slot_offset = (slot as usize).wrapping_sub(obj as usize);
    let card = slot_offset / crate::gc::CARD_SIZE;
    mark_card_range(card_table, card, card);
  }
}

/// Range write barrier for GC.
///
/// Called after a bulk write into `obj`.
///
/// - `start_slot` points within `obj` to the first written byte (typically the first pointer slot).
/// - `len` is the number of bytes written starting at `start_slot`.
///
/// This barrier is conservative and does not inspect the stored values; it may over-mark cards.
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier_range(
  obj: crate::roots::GcPtr,
  start_slot: *mut u8,
  len: usize,
) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier_range();

  if obj.is_null() || start_slot.is_null() || len == 0 {
    return;
  }

  // Avoid UB on misaligned pointers: `obj` must be a valid `ObjHeader` base pointer.
  if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
    std::process::abort();
  }

  // Writes into young objects don't need a barrier: nursery tracing will find the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  // Old-object bulk write. Mark the base object as remembered (idempotently) so
  // minor GC can consult its dirty cards and/or rescan it.
  remember_old_object(obj);

  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if card_table.is_null() {
    return;
  }

  let obj_addr = obj as usize;
  let start_addr = start_slot as usize;
  if start_addr < obj_addr {
    std::process::abort();
  }
  let start_offset = start_addr - obj_addr;

  if header.type_desc.is_null() {
    std::process::abort();
  }
  let obj_size = crate::gc::obj_size(obj);
  if start_offset >= obj_size {
    return;
  }

  let end_offset = start_offset.saturating_add(len).min(obj_size);
  if end_offset <= start_offset {
    return;
  }

  let start_card = start_offset / crate::gc::CARD_SIZE;
  let end_card = (end_offset - 1) / crate::gc::CARD_SIZE;
  mark_card_range(card_table, start_card, end_card);
}

#[cfg(test)]
mod write_barrier_tests {
  use super::*;

  // These tests mutate global write-barrier state (`YOUNG_SPACE`), so they must
  // not run concurrently under the default parallel Rust test runner.
  static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
  fn with_test_lock<T>(f: impl FnOnce() -> T) -> T {
    let _g = TEST_LOCK.lock();
    f()
  }

  #[repr(C)]
  struct DummyObject {
    header: ObjHeader,
    field: *mut u8,
  }

  fn clear_for_test() {
    // Clear global write-barrier state so tests don't leak configuration (young range + remset)
    // between them. Clearing the remset is required to avoid leaving dangling raw pointers when the
    // dummy objects are dropped at the end of the test.
    clear_write_barrier_state_for_tests();
  }

  #[test]
  fn write_barrier_records_old_to_young_edges() {
    with_test_lock(|| {
      clear_for_test();

      let mut young_byte = Box::new(0u8);
      let young_ptr = (&mut *young_byte) as *mut u8;
      unsafe {
        rt_gc_set_young_range(young_ptr, young_ptr.add(1));
      }

      let mut old = Box::new(DummyObject {
        header: ObjHeader {
          type_desc: std::ptr::null(),
          meta: std::sync::atomic::AtomicUsize::new(0),
        },
        field: young_ptr,
      });

      let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
      let slot_ptr = (&mut old.field) as *mut *mut u8 as *mut u8;
      unsafe {
        rt_write_barrier(obj_ptr, slot_ptr);
      }

      assert!(old.header.is_remembered());

      clear_for_test();
    });
  }

  #[test]
  fn write_barrier_range_records_old_to_young_edges() {
    with_test_lock(|| {
      clear_for_test();

      let mut young_byte = Box::new(0u8);
      let young_ptr = (&mut *young_byte) as *mut u8;
      unsafe {
        rt_gc_set_young_range(young_ptr, young_ptr.add(1));
      }

      #[repr(C)]
      struct DummyArray {
        header: ObjHeader,
        slots: [*mut u8; 4],
      }

      let mut old = Box::new(DummyArray {
        header: ObjHeader {
          type_desc: std::ptr::null(),
          meta: std::sync::atomic::AtomicUsize::new(0),
        },
        slots: [std::ptr::null_mut(); 4],
      });

      old.slots[2] = young_ptr;

      let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
      let start_slot = old.slots.as_mut_ptr() as *mut u8;
      let len = old.slots.len() * core::mem::size_of::<*mut u8>();
      unsafe {
        rt_write_barrier_range(obj_ptr, start_slot, len);
      }

      assert!(old.header.is_remembered());

      clear_for_test();
    });
  }
}

/// Trigger a GC cycle.
///
/// Current milestone runtime:
/// - Performs a cooperative stop-the-world handshake across registered threads.
/// - Invokes the stackmap-based root enumeration hook (if stackmaps are available).
/// - Does *not* yet run a full GC algorithm (mark/copy/etc).
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_gc_collect() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_gc_collect();

  let res = catch_unwind(AssertUnwindSafe(|| {
    // If a stop-the-world is already active, join it as a mutator safepoint at
    // this callsite (so we still publish a safepoint context for stack walking).
    let epoch = crate::threading::safepoint::current_epoch();
    if epoch & 1 == 1 {
      crate::safepoint::enter_safepoint_at_current_callsite(epoch);
      return;
    }

    // Attempt to become the stop-the-world coordinator.
    let Some(stop_epoch) = crate::threading::safepoint::rt_gc_try_request_stop_the_world() else {
      // Lost the race; if a GC is now active, join it.
      let epoch = crate::threading::safepoint::current_epoch();
      if epoch & 1 == 1 {
        crate::safepoint::enter_safepoint_at_current_callsite(epoch);
      }
      return;
    };

    // If `rt_gc_collect` is called from an attached mutator thread, publish the
    // initiator's safepoint context before waiting for other threads. This keeps
    // the initiator's stack eligible for stackmap-based root enumeration while
    // the world is stopped.
    if registry::current_thread_id().is_some() {
      // If GC is triggered while executing inside runtime code (e.g. `rt_alloc`
      // decides to collect), the nearest managed frame is suspended at the
      // callsite into the *outermost* runtime frame (which has a stackmap
      // record). Recover that managed callsite cursor by walking the FP chain.
      let ctx = crate::stackmap::try_stackmaps()
        .and_then(|stackmaps| crate::stackwalk::find_nearest_managed_cursor_from_here(stackmaps))
        .map(|cursor| {
          let sp_callsite = cursor.sp.unwrap_or(0);
          #[cfg(target_arch = "x86_64")]
          let sp_entry = sp_callsite.saturating_sub(crate::arch::WORD_SIZE as u64);
          #[cfg(not(target_arch = "x86_64"))]
          let sp_entry = sp_callsite;

          crate::arch::SafepointContext {
            sp_entry: sp_entry as usize,
            sp: sp_callsite as usize,
            fp: cursor.fp as usize,
            ip: cursor.pc as usize,
          }
        })
        .unwrap_or_else(crate::arch::capture_safepoint_context);
      registry::set_current_thread_safepoint_context(ctx);
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);
      crate::threading::safepoint::notify_state_change();
    }

    crate::safepoint::with_world_stopped_requested(stop_epoch, || {});
  }));

  if res.is_err() {
    std::process::abort();
  }
}

/// Returns the total number of bytes currently held in non-moving backing stores (e.g. `ArrayBuffer`
/// bytes) allocated outside the GC heap.
///
/// This value is intended for memory-pressure heuristics: large external buffers should contribute
/// to GC trigger decisions even though they are not part of the moving heap.
#[no_mangle]
pub extern "C" fn rt_backing_store_external_bytes() -> usize {
  crate::buffer::backing_store::global_backing_store_allocator().external_bytes()
}

// -----------------------------------------------------------------------------
// Per-thread shadow stack roots (temporary roots)
// -----------------------------------------------------------------------------

/// Push a root slot onto the current thread's shadow stack.
///
/// The slot address is stored in the per-thread handle stack inside the runtime thread registry so
/// the stop-the-world GC can enumerate and update it during relocation.
///
/// # Safety
/// - `slot` must be a valid, writable pointer to a `GcPtr` slot (`*mut *mut u8`).
/// - Callers must later pop the slot in strict LIFO order (see [`rt_root_pop`]).
/// - The current thread must be registered with `rt_thread_init`.
#[no_mangle]
pub unsafe extern "C" fn rt_root_push(slot: crate::roots::GcHandle) {
  if slot.is_null() {
    crate::trap::rt_trap_invalid_arg("rt_root_push: slot was null");
  }
  let Some(thread) = registry::current_thread_state() else {
    crate::trap::rt_trap_invalid_arg("rt_root_push: current thread is not registered");
  };
  thread.handle_stack_push(slot);
}

/// Pop a root slot from the current thread's shadow stack.
///
/// In debug builds this enforces strict LIFO order.
///
/// # Safety
/// - `slot` must be the most recently pushed slot on the current thread.
/// - The current thread must be registered with `rt_thread_init`.
#[no_mangle]
pub unsafe extern "C" fn rt_root_pop(slot: crate::roots::GcHandle) {
  if slot.is_null() {
    crate::trap::rt_trap_invalid_arg("rt_root_pop: slot was null");
  }
  let Some(thread) = registry::current_thread_state() else {
    crate::trap::rt_trap_invalid_arg("rt_root_pop: current thread is not registered");
  };
  thread.handle_stack_pop_debug(slot);
}

// -----------------------------------------------------------------------------
// Global roots / handles (non-stack roots)
// -----------------------------------------------------------------------------

/// Register a global/static root slot.
///
/// This is a convenience wrapper for codegen/FFI that stores GC pointers in raw word slots
/// (`usize`). The slot address is added to the always-scanned global root set, allowing a moving GC
/// to update the slot in-place.
#[no_mangle]
pub extern "C" fn rt_global_root_register(slot: *mut usize) {
  crate::roots::register_global_root_slot(slot);
}

/// Unregister a global/static root slot previously registered via [`rt_global_root_register`].
#[no_mangle]
pub extern "C" fn rt_global_root_unregister(slot: *mut usize) {
  crate::roots::unregister_global_root_slot(slot);
}

/// Register an addressable root slot with the runtime.
///
/// `slot` must point to a writable `*mut u8` and must remain valid until the
/// returned handle is passed to [`rt_gc_unregister_root_slot`].
#[no_mangle]
pub extern "C" fn rt_gc_register_root_slot(slot: crate::roots::GcHandle) -> u32 {
  crate::roots::global_root_registry().register_root_slot(slot)
}

/// Unregister a previously registered root slot handle.
#[no_mangle]
pub extern "C" fn rt_gc_unregister_root_slot(handle: u32) {
  crate::roots::global_root_registry().unregister(handle);
}

/// Convenience API: create an internal root slot initialized to `ptr`.
///
/// This is primarily intended for FFI/host embeddings that want a persistent
/// handle without managing slot storage themselves.
#[no_mangle]
pub extern "C" fn rt_gc_pin(ptr: crate::roots::GcPtr) -> u32 {
  crate::roots::global_root_registry().pin(ptr)
}

/// Destroy a handle created by [`rt_gc_pin`].
#[no_mangle]
pub extern "C" fn rt_gc_unpin(handle: u32) {
  crate::roots::global_root_registry().unregister(handle);
}

// -----------------------------------------------------------------------------
// Persistent handle IDs (stable u64)
// -----------------------------------------------------------------------------
//
// These are stable integer IDs intended for crossing async / OS / thread boundaries (epoll/kqueue
// userdata, cross-thread wakeups, ...). They are backed by `roots::RootRegistry` entries so the GC
// can update the underlying slot when objects move.

/// Allocate a new persistent handle rooting `ptr`.
#[no_mangle]
pub extern "C" fn rt_handle_alloc(ptr: *mut u8) -> u64 {
  crate::roots::global_root_registry().pin(ptr) as u64
}

/// Free a persistent handle created by [`rt_handle_alloc`].
///
/// Invalid handles are ignored.
#[no_mangle]
pub extern "C" fn rt_handle_free(handle: u64) {
  let Ok(handle) = u32::try_from(handle) else {
    return;
  };
  crate::roots::global_root_registry().unregister(handle);
}

/// Resolve a persistent handle back to the (possibly relocated) pointer stored in its slot.
///
/// Returns null if the handle is invalid or has been freed.
#[no_mangle]
pub extern "C" fn rt_handle_load(handle: u64) -> *mut u8 {
  let Ok(handle) = u32::try_from(handle) else {
    return std::ptr::null_mut();
  };
  crate::roots::global_root_registry()
    .get(handle)
    .unwrap_or(std::ptr::null_mut())
}

/// Update the pointer stored in a persistent handle slot.
///
/// Invalid handles are ignored.
#[no_mangle]
pub extern "C" fn rt_handle_store(handle: u64, ptr: *mut u8) {
  let Ok(handle) = u32::try_from(handle) else {
    return;
  };
  let _ = crate::roots::global_root_registry().set(handle, ptr);
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_stats_snapshot(out: *mut crate::abi::RtGcStatsSnapshot) {
  if out.is_null() {
    return;
  }
  *out = crate::gc_stats::snapshot();
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub extern "C" fn rt_gc_stats_reset() {
  crate::gc_stats::reset();
}

// -----------------------------------------------------------------------------
// Weak handles (non-owning references)
// -----------------------------------------------------------------------------

/// Create a new weak handle for `value`.
///
/// Weak handles do not keep the referent alive. If the referent is collected, `rt_weak_get`
/// returns null.
#[no_mangle]
pub extern "C" fn rt_weak_add(value: crate::roots::GcPtr) -> u64 {
  crate::gc::weak::global_weak_add(value).as_u64()
}

/// Resolve a weak handle back to a pointer, or null if the referent is dead/cleared.
#[no_mangle]
pub extern "C" fn rt_weak_get(handle: u64) -> crate::roots::GcPtr {
  crate::gc::weak::global_weak_get(WeakHandle::from_u64(handle)).unwrap_or(std::ptr::null_mut())
}

/// Remove a weak handle.
#[no_mangle]
pub extern "C" fn rt_weak_remove(handle: u64) {
  crate::gc::weak::global_weak_remove(WeakHandle::from_u64(handle));
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().spawn(task, data)
  }));
  match res {
    Ok(id) => id,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(tasks: *const TaskId, count: usize) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().join(tasks, count)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_for(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().parallel_for(start, end, body, data)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

/// Spawn CPU-bound work on the work-stealing pool, returning a promise that can be awaited by the
/// async runtime.
///
/// The spawned `task` must:
/// 1. Write its result into `rt_promise_payload_ptr(promise)` (respecting `promise_layout`), then
/// 2. Settle the promise (usually via `rt_promise_fulfill`).
///
/// Note: unlike `rt_parallel_spawn`, this API is *detached* and does not require `rt_parallel_join`;
/// completion is observed by awaiting the returned promise.
#[no_mangle]
pub extern "C" fn rt_parallel_spawn_promise(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_layout: PromiseLayout,
) -> PromiseRef {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::parallel_integration::spawn_promise(task, data, promise_layout)
  }));
  match res {
    Ok(p) => p,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_spawn_blocking(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
) -> PromiseRef {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    crate::blocking_pool::spawn(task, data)
  })
}

#[no_mangle]
pub extern "C" fn rt_async_spawn_legacy(coro: *mut RtCoroutineHeader) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    async_rt::coroutine::async_spawn(coro)
  })
}

/// Like [`rt_async_spawn_legacy`], but enqueues the coroutine's first resume as a microtask instead
/// of running it synchronously.
///
/// This is required for Web-style microtask semantics (e.g. `queueMicrotask`).
#[no_mangle]
pub extern "C" fn rt_async_spawn_deferred_legacy(coro: *mut RtCoroutineHeader) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    async_rt::coroutine::async_spawn_deferred(coro)
  })
}

/// Cancel all runtime-owned async-ABI coroutine frames currently queued in the runtime.
///
/// This is primarily a teardown helper: it is intended to be called when the host is shutting down
/// and wants to ensure no heap-owned coroutine frames leak.
#[no_mangle]
pub extern "C" fn rt_async_cancel_all() {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::async_runtime::cancel_all();
  })
}

/// Drive the runtime's async/event-loop queues.
///
/// This runtime maintains process-global singleton state. `rt_async_poll_legacy` may be called from
/// multiple threads, but calls are **globally serialized** (only one thread executes the poll loop
/// at a time).
///
/// Returns `true` if there is still pending work after this poll turn (queued tasks, active
/// timers, or I/O watchers). Returns `false` when the runtime is quiescent.
#[no_mangle]
pub extern "C" fn rt_async_poll_legacy() -> bool {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    if async_runtime::has_error() {
      return false;
    }
    let pending = async_rt::poll();
    if async_runtime::has_error() {
      false
    } else {
      pending
    }
  })
}

#[no_mangle]
pub extern "C" fn rt_async_set_limits(max_steps: usize, max_queue_len: usize) {
  abort_on_panic(|| {
    async_runtime::set_limits(max_steps, max_queue_len);
  })
}

#[no_mangle]
pub extern "C" fn rt_async_take_last_error() -> *mut c_char {
  abort_on_panic(|| match async_runtime::take_last_error() {
    None => std::ptr::null_mut(),
    Some(msg) => CString::new(msg)
      .unwrap_or_else(|_| CString::new("async executor error").unwrap())
      .into_raw(),
  })
}

#[no_mangle]
pub unsafe extern "C" fn rt_async_free_c_string(s: *mut c_char) {
  abort_on_panic(|| {
    if s.is_null() {
      return;
    }
    unsafe {
      drop(CString::from_raw(s));
    }
  })
}

/// Configure whether `await` on an already-settled promise yields to the microtask queue (strict JS
/// semantics) or resumes synchronously (fast-path).
///
/// Default is `false` (fast-path).
#[no_mangle]
pub extern "C" fn rt_async_set_strict_await_yields(strict: bool) {
  abort_on_panic(|| {
    async_rt::set_strict_await_yields(strict);
  })
}

/// Block the current thread until at least one async task becomes ready.
///
/// This allows an event-loop thread to park when the runtime is idle (no timers
/// or I/O watchers) and be woken by promise settlement or other cross-thread
/// enqueues.
#[no_mangle]
pub extern "C" fn rt_async_wait() {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    async_rt::wait_for_work();
  })
}

/// Drive the executor until there is no immediately-ready work remaining (microtask checkpoint).
///
/// Returns `true` if any work was executed, `false` if the runtime was already idle.
#[export_name = "rt_async_run_until_idle"]
pub unsafe extern "C" fn rt_async_run_until_idle_abi() -> bool {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::async_runtime::rt_async_run_until_idle()
  })
}

/// Block the current thread until `p` is settled.
///
/// This is a convenience helper for generated programs (and embedders) to drive the runtime
/// without re-implementing the poll/wait loop.
#[no_mangle]
pub unsafe extern "C" fn rt_async_block_on(p: PromiseRef) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();

    // Fast path: already settled.
    if !promise_is_pending(p) {
      return;
    }

    // Ensure the event loop is woken when `p` is settled even if nothing else is
    // awaiting it. Without this, `rt_async_wait` can sleep indefinitely.
    register_block_on_waker(p);

    loop {
      let _ = crate::async_runtime::rt_async_run_until_idle();

      if !promise_is_pending(p) {
        return;
      }

      // No ready work; park until something wakes the runtime.
      async_rt::wait_for_work();
    }
  })
}

#[no_mangle]
pub extern "C" fn rt_async_sleep_legacy(delay_ms: u64) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();

    extern "C" fn resolve_sleep(data: *mut u8) {
      let promise = PromiseRef(data.cast());
      async_rt::promise::promise_resolve(promise, core::ptr::null_mut());
    }

    let promise = async_rt::promise::promise_new();
    let _timer_id = async_rt::global().schedule_timer_in(
      std::time::Duration::from_millis(delay_ms),
      async_rt::Task::new(resolve_sleep, promise.0 as *mut u8),
    );
    promise
  })
}

// -----------------------------------------------------------------------------
// I/O readiness watchers (reactor-backed)
// -----------------------------------------------------------------------------

fn maybe_log_rt_io_failure(op: &str, msg: impl core::fmt::Display) {
  // These functions are part of the stable C ABI surface, but they cannot return
  // a rich error (e.g. `rt_io_register` returns 0 on failure). Emit a best-effort
  // diagnostic to stderr in debug builds so failures (like registering a blocking
  // fd) are diagnosable.
  if cfg!(debug_assertions) {
    eprintln!("runtime-native: {op} failed: {msg}");
  }
}

/// Register an fd with the runtime's readiness reactor.
///
/// ## Nonblocking / edge-triggered contract
///
/// The runtime-native reactor uses **edge-triggered** readiness notifications. The
/// provided `fd` **must already be set to `O_NONBLOCK`** before calling this
/// function.
///
/// On failure, this function returns `0`. In debug builds, failures are logged to
/// stderr to aid diagnosis.
#[no_mangle]
pub extern "C" fn rt_io_register(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
) -> IoWatcherId {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    match async_rt::global().register_io(fd, interests, cb, data) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        if err.kind() == io::ErrorKind::InvalidInput {
          maybe_log_rt_io_failure(
            "rt_io_register",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Update the interest mask for an I/O watcher created by [`rt_io_register`].
///
/// If the watcher is invalid or the underlying fd no longer satisfies the
/// nonblocking contract, the update is ignored. In debug builds, failures are
/// logged to stderr.
#[no_mangle]
pub extern "C" fn rt_io_update(id: IoWatcherId, interests: u32) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    if !async_rt::global().update_io(WatcherId::from_raw(id), interests) {
      maybe_log_rt_io_failure(
        "rt_io_update",
        format_args!(
          "id={id} interests=0x{interests:x}: update failed (invalid id or fd no longer nonblocking)"
        ),
      );
    }
  })
}

/// Unregister an I/O watcher created by [`rt_io_register`].
///
/// If the watcher is invalid, this is a no-op. In debug builds, failures are
/// logged to stderr.
#[no_mangle]
pub extern "C" fn rt_io_unregister(id: IoWatcherId) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    if !async_rt::global().deregister_fd(WatcherId::from_raw(id)) {
      maybe_log_rt_io_failure(
        "rt_io_unregister",
        format_args!("id={id}: unregister failed (invalid id)"),
      );
    }
  })
}

// -----------------------------------------------------------------------------
// Microtasks + timers (queueMicrotask/setTimeout/setInterval)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebTimerKind {
  Timeout,
  Interval,
}

#[derive(Clone, Copy)]
struct WebTimerState {
  kind: WebTimerKind,
  cb: async_rt::TaskFn,
  data: *mut u8,
  drop_data: Option<async_rt::TaskDropFn>,
  interval: Duration,
  internal_id: async_rt::TimerId,
  /// True while the timer's callback is executing.
  ///
  /// This allows `rt_clear_timer` to defer dropping callback state when a callback clears its own
  /// interval (JS-style `clearInterval` from within the interval callback).
  firing: bool,
  /// Interval cancellation requested while `firing == true`.
  cancelled: bool,
}

// Safety: `WebTimerState` is stored behind a mutex in a process-global map and contains only opaque
// pointers + Copy types. The runtime never dereferences `data`; it is passed back to user callbacks
// on the event-loop thread. Allowing it to cross thread boundaries is therefore safe as far as the
// runtime is concerned (FFI callers are responsible for ensuring their pointers remain valid).
unsafe impl Send for WebTimerState {}

static NEXT_WEB_TIMER_ID: AtomicU64 = AtomicU64::new(1);
static WEB_TIMERS: Lazy<Mutex<HashMap<TimerId, WebTimerState>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn clear_web_timers_for_tests() {
  let mut timers = WEB_TIMERS.lock();
  for (_, st) in timers.drain() {
    if let Some(drop_data) = st.drop_data {
      (drop_data)(st.data);
    }
  }
}

fn alloc_web_timer_id() -> TimerId {
  loop {
    let id = NEXT_WEB_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    if id != 0 {
      return id;
    }
  }
}

fn timer_id_to_ptr(id: TimerId) -> *mut u8 {
  id as usize as *mut u8
}

fn timer_id_from_ptr(data: *mut u8) -> TimerId {
  data as usize as TimerId
}

extern "C" fn web_timer_fire(data: *mut u8) {
  let id = timer_id_from_ptr(data);

  let (kind, cb, cb_data, interval, drop_data) = {
    let mut timers = WEB_TIMERS.lock();
    let Some(snapshot) = timers.get(&id).copied() else {
      return;
    };

    match snapshot.kind {
      WebTimerKind::Timeout => {
        let st = timers.remove(&id).expect("timer entry disappeared");
        (WebTimerKind::Timeout, st.cb, st.data, Duration::ZERO, st.drop_data)
      }
      WebTimerKind::Interval => {
        let st = timers.get_mut(&id).expect("timer entry disappeared");
        st.firing = true;
        (WebTimerKind::Interval, snapshot.cb, snapshot.data, snapshot.interval, snapshot.drop_data)
      }
    }
  };

  (cb)(cb_data);

  if kind == WebTimerKind::Timeout {
    if let Some(drop_data) = drop_data {
      drop_data(cb_data);
    }
    return;
  }

  // Reschedule interval if it is still active after the callback.
  let drop_after = {
    let mut timers = WEB_TIMERS.lock();
    let Some(st) = timers.get_mut(&id) else {
      return;
    };
    if st.kind != WebTimerKind::Interval {
      return;
    }
    st.firing = false;

    // If the callback cleared the interval, tear down the callback state now.
    if st.cancelled {
      let st = timers.remove(&id).expect("timer entry disappeared");
      st.drop_data.map(|f| (f, st.data))
    } else {
      None
    }
  };

  if let Some((drop_data, cb_data)) = drop_after {
    drop_data(cb_data);
    return;
  }

  // HTML clamps nested timers to >= 4ms after a nesting depth of 5. The native runtime does not
  // currently track nesting; higher layers can implement clamping policy if needed.
  let deadline = Instant::now().checked_add(interval).unwrap_or_else(Instant::now);
  let task = async_rt::Task::new(web_timer_fire, data);
  {
    let mut timers = WEB_TIMERS.lock();
    let Some(st) = timers.get_mut(&id) else {
      return;
    };
    if st.kind != WebTimerKind::Interval || st.cancelled {
      return;
    }
    st.internal_id = async_rt::global().schedule_timer(deadline, task);
  };
}

#[no_mangle]
pub extern "C" fn rt_queue_microtask(cb: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::enqueue_microtask(cb, data);
  })
}

#[no_mangle]
pub extern "C" fn rt_queue_microtask_with_drop(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::global().enqueue_microtask(async_rt::Task::new_with_drop(cb, data, drop_data));
  })
}

#[no_mangle]
pub extern "C" fn rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let deadline = Instant::now().checked_add(delay).unwrap_or_else(Instant::now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb,
        data,
        drop_data: None,
        interval: Duration::ZERO,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

#[no_mangle]
pub extern "C" fn rt_set_timeout_with_drop(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
  delay_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let deadline = Instant::now().checked_add(delay).unwrap_or_else(Instant::now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb,
        data,
        drop_data: Some(drop_data),
        interval: Duration::ZERO,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

#[no_mangle]
pub extern "C" fn rt_set_interval(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  interval_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let deadline = Instant::now().checked_add(interval).unwrap_or_else(Instant::now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb,
        data,
        drop_data: None,
        interval,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

#[no_mangle]
pub extern "C" fn rt_set_interval_with_drop(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
  interval_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let deadline = Instant::now().checked_add(interval).unwrap_or_else(Instant::now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb,
        data,
        drop_data: Some(drop_data),
        interval,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

#[no_mangle]
pub extern "C" fn rt_clear_timer(id: TimerId) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let (st, should_drop) = {
      let mut timers = WEB_TIMERS.lock();
      let Some(st) = timers.get(&id).copied() else {
        return;
      };
      if st.kind == WebTimerKind::Interval && st.firing {
        let st = timers.get_mut(&id).expect("timer entry disappeared");
        st.cancelled = true;
        return;
      }
      let st = timers.remove(&id).expect("timer entry disappeared");
      (st, st.drop_data.map(|f| (f, st.data)))
    };
    let _ = async_rt::global().cancel_timer(st.internal_id);
    if let Some((drop_data, cb_data)) = should_drop {
      drop_data(cb_data);
    }
  })
}

// -----------------------------------------------------------------------------
// Minimal promise ABI (used by async/await lowering)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_promise_new_legacy() -> PromiseRef {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_new()
  })
}

/// Return the payload buffer associated with a promise created by `rt_parallel_spawn_promise`.
///
/// For non-payload promises, this may return null.
#[no_mangle]
pub extern "C" fn rt_promise_payload_ptr(p: PromiseRef) -> *mut u8 {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_payload_ptr(p)
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_legacy(p: PromiseRef, value: ValueRef) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_resolve(p, value)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_reject_legacy(p: PromiseRef, err: ValueRef) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_reject(p, err)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_into_legacy(p: PromiseRef, value: PromiseResolveInput) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_resolve_into(p, value)
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_promise_legacy(p: PromiseRef, other: PromiseRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_resolve_promise(p, other)
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_thenable_legacy(p: PromiseRef, thenable: ThenableRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_resolve_thenable(p, thenable)
}

#[no_mangle]
pub extern "C" fn rt_promise_then_legacy(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_then(p, on_settle, data)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_then_with_drop_legacy(
  p: PromiseRef,
  on_settle: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_then_with_drop(p, on_settle, data, drop_data)
  })
}

#[no_mangle]
pub extern "C" fn rt_coro_await_legacy(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::coroutine::coro_await(coro, awaited, next_state)
  })
}

#[no_mangle]
pub extern "C" fn rt_coro_await_value_legacy(coro: *mut RtCoroutineHeader, awaited: PromiseResolveInput, next_state: u32) {
  ensure_event_loop_thread_registered();
  async_rt::coroutine::coro_await_value(coro, awaited, next_state)
}

// -----------------------------------------------------------------------------
// Thread registration (native codegen / embedding)
// -----------------------------------------------------------------------------

/// Attach the calling OS thread to `runtime`.
///
/// Returns a pointer to the per-thread [`Thread`] record, or null on failure.
///
/// # Safety
/// `runtime` must be a valid pointer to a [`Runtime`] created by the embedder.
#[no_mangle]
pub unsafe extern "C" fn rt_thread_attach(runtime: *mut Runtime) -> *mut Thread {
  let Some(runtime) = runtime.as_ref() else {
    return std::ptr::null_mut();
  };

  match runtime.attach_current_thread_raw() {
    Ok(thread) => thread,
    Err(_) => std::ptr::null_mut(),
  }
}

/// Detach the calling OS thread from its runtime.
///
/// This must be invoked on the *same* OS thread that previously called
/// [`rt_thread_attach`].
///
/// If `thread` is invalid, already detached, or not the current thread, this is
/// a no-op.
///
/// # Safety
/// `thread` must be a pointer previously returned by [`rt_thread_attach`].
#[no_mangle]
pub unsafe extern "C" fn rt_thread_detach(thread: *mut Thread) {
  let Some(thread_ref) = thread.as_ref() else {
    return;
  };

  let runtime = thread_ref.runtime;
  let Some(runtime) = runtime.as_ref() else {
    return;
  };

  // Best-effort: we cannot report errors over this C ABI.
  let _ = runtime.detach_thread_ptr(thread);
}
