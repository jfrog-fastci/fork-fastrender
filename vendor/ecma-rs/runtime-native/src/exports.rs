use crate::abi::Microtask;
use crate::abi::PromiseRef;
use crate::abi::PromiseResolveInput;
use crate::abi::RtCoroutineHeader;
use crate::abi::RtShapeId;
use crate::abi::RtThreadKind;
use crate::abi::TaskId;
use crate::abi::TimerId;
use crate::abi::ThenableRef;
use crate::abi::ValueRef;
use crate::abi::IoWatcherId;
use crate::async_runtime::PromiseLayout;
use crate::array;
use crate::array::RtArrayHeader;
use crate::async_runtime;
use crate::async_rt;
use crate::async_rt::WatcherId;
use crate::ffi::abort_on_panic;
use crate::async_abi::PromiseHeader;
use crate::gc::global_remset;
use crate::gc::HandleId;
use crate::gc::ObjHeader;
use crate::gc::TypeDescriptor;
use crate::gc::WeakHandle;
use crate::gc::YOUNG_SPACE;
use crate::BackingStoreAllocator;
#[cfg(feature = "gc_stats")]
use crate::abi::RtGcStatsSnapshot;
use crate::sync::GcAwareMutex;
use crate::threading;
use crate::threading::registry;
use crate::trap;
use crate::Runtime;
use crate::Thread;
use once_cell::sync::Lazy;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Serialize `rt_gc_collect` calls so concurrent initiators coalesce into a single stop-the-world
/// cycle.
///
/// This must be a GC-aware lock: a mutator thread blocked attempting to trigger a collection should
/// not prevent another thread from reaching a stop-the-world safepoint.
static GC_COLLECT_MUTEX: Lazy<GcAwareMutex<()>> = Lazy::new(|| GcAwareMutex::new(()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GcCollectKind {
  Minor,
  Major,
}

impl GcCollectKind {
  #[inline]
  fn satisfied_epoch(self) -> u64 {
    match self {
      // Minor GC always bumps the nursery epoch (even when the nursery is empty),
      // and major GC begins with a minor GC. Treat any nursery-epoch change as
      // satisfying a "minor GC requested" call.
      Self::Minor => crate::rt_alloc::NURSERY_EPOCH.load(Ordering::Relaxed),
      // Major GC bumps the major epoch once it has completed. Do not treat a
      // concurrent minor GC or other stop-the-world phase as satisfying a major
      // request.
      Self::Major => crate::rt_alloc::MAJOR_EPOCH.load(Ordering::Relaxed),
    }
  }

  #[inline]
  fn oom_trap_msg(self, entry_name: &'static str) -> &'static str {
    match self {
      // Keep the trap message stable and descriptive for debugging.
      Self::Minor => "rt_gc_collect_minor: minor collection failed",
      // Preserve the existing message for the legacy `rt_gc_collect` entrypoint.
      Self::Major => match entry_name {
        "rt_gc_collect" => "rt_gc_collect: major collection failed",
        _ => "rt_gc_collect_major: major collection failed",
      },
    }
  }
}

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

  // PromiseHeader stores the reaction/waiter list head in `waiters`.
  let waiters = unsafe { &(*promise).waiters };
  loop {
    let head_val = waiters.load(Ordering::Acquire);
    let head = crate::promise_reactions::decode_waiters_ptr(head_val);
    unsafe {
      (*node).next = head;
    }
    if waiters
      .compare_exchange(head_val, node as usize, Ordering::AcqRel, Ordering::Acquire)
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

  // PromiseHeader stores the reaction/waiter list head in `waiters`.
  let waiters = unsafe { &(*promise).waiters };
  let head_val = waiters.swap(0, Ordering::AcqRel);
  let mut head = crate::promise_reactions::decode_waiters_ptr(head_val);
  if head.is_null() {
    async_rt::promise::untrack_pending_reactions(promise);
    return;
  }

  async_rt::promise::untrack_pending_reactions(promise);

  // Preserve FIFO registration order.
  head = unsafe { crate::promise_reactions::reverse_list(head) };

  crate::promise_reactions::enqueue_reaction_jobs(promise, head);
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
  if unsafe { &*promise }.mark_handled() {
    crate::unhandled_rejection::on_handle(p);
  }

  let node = alloc_block_on_reaction();
  push_reaction(promise, node);
  async_rt::promise::track_pending_reactions(promise);

  // If the promise is already settled, drain and schedule immediately.
  let state = unsafe { &(*promise).state }.load(Ordering::Acquire);
  if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
    drain_reactions(promise);
  }
}

#[inline(always)]
fn ensure_event_loop_thread_registered() {
  // The legacy async runtime is driven by a single-consumer event loop. Ensure
  // the calling thread is registered so stop-the-world GC coordination does not
  // ignore its stack.
  crate::async_rt::ensure_event_loop_thread();
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
  // This helper is used by the write barrier, which must be panic-free and
  // should never rely on debug assertions for correctness.
  if card_table.is_null() || start_card > end_card {
    std::process::abort();
  }

  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_card_marks((end_card - start_card + 1) as u64);

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
      (*card_table.add(start_word)).fetch_or(mask, Ordering::Relaxed);
      return;
    }

    // First word: mark from start_bit..=63.
    (*card_table.add(start_word)).fetch_or((!0u64) << start_bit, Ordering::Relaxed);

    // Middle words: mark all bits.
    for word in (start_word + 1)..end_word {
      (*card_table.add(word)).fetch_or(!0u64, Ordering::Relaxed);
    }

    // Last word: mark 0..=end_bit.
    let last_mask = if end_bit == 63 {
      !0u64
    } else {
      (1u64 << (end_bit + 1)) - 1
    };
    (*card_table.add(end_word)).fetch_or(last_mask, Ordering::Relaxed);
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
  // Capture the frame pointer of this runtime entrypoint before entering `abort_on_panic`, which
  // uses `catch_unwind` in `panic=unwind` builds and may not preserve a reliable frame-pointer chain.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_alloc(size);
    crate::rt_alloc::alloc(size, shape, entry_fp)
  })
}

/// Allocate a pinned (non-moving) GC object.
///
/// Pinned objects live in the runtime's non-moving space and will not be relocated during GC.
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_alloc_pinned(size: usize, shape: RtShapeId) -> crate::roots::GcPtr {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| crate::rt_alloc::alloc_pinned(size, shape, entry_fp))
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> crate::roots::GcPtr {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| {
    let Some(spec) = array::decode_rt_array_elem_size(elem_size) else {
      crate::trap::rt_trap_invalid_arg("rt_alloc_array: invalid elem_size");
    };
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_alloc_array(len, spec.elem_size);
    let _ = spec;
    crate::rt_alloc::alloc_array(len, elem_size, entry_fp)
  })
}

#[no_mangle]
pub extern "C" fn rt_alloc_ptr_array(len: usize) -> *mut u8 {
  abort_on_panic(|| rt_alloc_array(len, array::RT_ARRAY_ELEM_PTR_FLAG | core::mem::size_of::<*mut u8>()))
}

#[no_mangle]
pub extern "C" fn rt_array_len(obj: *mut u8) -> usize {
  abort_on_panic(|| {
    if obj.is_null() {
      trap::rt_trap_invalid_arg("rt_array_len called with null");
    }
    // SAFETY: The ABI contract requires `obj` be a valid array object.
    unsafe {
      let mut obj = obj;
      let header = &*crate::gc::header_from_obj(obj);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
      }
      let header = &*crate::gc::header_from_obj(obj);
      if header.type_desc != &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
        trap::rt_trap_invalid_arg("rt_array_len called on non-array object");
      }
      (*(obj as *const RtArrayHeader)).len
    }
  })
}

#[no_mangle]
pub extern "C" fn rt_array_data(obj: *mut u8) -> *mut u8 {
  abort_on_panic(|| {
    if obj.is_null() {
      trap::rt_trap_invalid_arg("rt_array_data called with null");
    }
    // SAFETY: The ABI contract requires `obj` be a valid array object.
    unsafe {
      let mut obj = obj;
      let header = &*crate::gc::header_from_obj(obj);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
      }
      let header = &*crate::gc::header_from_obj(obj);
      if header.type_desc != &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
        trap::rt_trap_invalid_arg("rt_array_data called on non-array object");
      }
      obj.add(array::RT_ARRAY_DATA_OFFSET)
    }
  })
}

/// Register the current OS thread with the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_init(kind: u32) {
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_thread_init();

    // Ensure the global GC heap is initialized outside stop-the-world GC. Tests assert that
    // `rt_gc_collect` performs no lazy allocations after thread init.
    crate::rt_alloc::ensure_global_heap_init();
    threading::register_current_thread(thread_kind_from_abi(kind));
  })
}

/// Unregister the current OS thread from the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_deinit() {
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_thread_deinit();
    threading::unregister_current_thread();
  })
}

/// Register the current OS thread with the runtime thread registry.
///
/// This is a stable compiler/runtime ABI entrypoint used by native codegen and
/// embedders. Returns a runtime-assigned thread id (stable for the lifetime of
/// the registration).
#[no_mangle]
pub extern "C" fn rt_thread_register(kind: RtThreadKind) -> u64 {
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_thread_init();

    // Match `rt_thread_init`: make sure GC heap singletons are created before any stop-the-world
    // collection is requested.
    crate::rt_alloc::ensure_global_heap_init();

    let kind = match kind {
      RtThreadKind::RT_THREAD_MAIN => threading::ThreadKind::Main,
      RtThreadKind::RT_THREAD_WORKER => threading::ThreadKind::Worker,
      RtThreadKind::RT_THREAD_IO => threading::ThreadKind::Io,
      RtThreadKind::RT_THREAD_EXTERNAL => threading::ThreadKind::External,
    };
    threading::register_current_thread(kind).get()
  })
}

/// Unregister the current OS thread from the runtime thread registry.
#[no_mangle]
pub extern "C" fn rt_thread_unregister() {
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_thread_deinit();

    threading::unregister_current_thread();
  })
}

/// Mark/unmark the current thread as parked (idle) inside the runtime.
///
/// When transitioning back to `parked == false` (unparking), this will perform a
/// safepoint poll before returning.
#[no_mangle]
pub extern "C" fn rt_thread_set_parked(parked: bool) {
  abort_on_panic(|| {
    threading::set_parked(parked);
  })
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
  // Capture the frame pointer of this runtime entrypoint *before* entering
  // `abort_on_panic` (which uses `catch_unwind` in `panic=unwind` builds).
  //
  // The sysroot's `catch_unwind` implementation may not maintain a valid frame
  // pointer chain, so safepoint slow paths may not be able to walk from the
  // closure frame back out to the managed callsite. Providing this FP as an
  // override keeps stackmap-based scanning robust.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| unsafe {
    if slot.is_null() {
      crate::trap::rt_trap_invalid_arg("rt_gc_safepoint_relocate_h: slot was null");
    }
    // Poll the stop-the-world barrier. Use the threading safepoint poll so the
    // fast path is a single epoch load and so the slow path can recover the
    // nearest managed callsite when this helper is invoked from runtime frames.
    crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
      crate::threading::safepoint::rt_gc_safepoint();
    });
    crate::roots::load_handle(slot)
  })
}

/// Cheap GC poll used by compiler-inserted fast paths (e.g. loop backedge safepoints).
///
/// Returns `true` if a stop-the-world GC/safepoint is currently requested.
///
/// Compiler-generated code should typically **not** call this function directly. Instead, it should
/// inline an atomic (Acquire) load of the exported `RT_GC_EPOCH` symbol and, on an odd value, call
/// `rt_gc_safepoint_slow(epoch)` at the *callsite* so `rewrite-statepoints-for-gc` can rewrite it
/// into a statepoint and the runtime captures the managed callsite context correctly.
///
/// `native-js` marks this function as a GC leaf (`"gc-leaf-function"`) so
/// `rewrite-statepoints-for-gc` does not wrap the poll itself in a statepoint.
#[no_mangle]
pub extern "C" fn rt_gc_poll() -> bool {
  abort_on_panic(|| (crate::threading::safepoint::RT_GC_EPOCH.load(Ordering::Acquire) & 1) != 0)
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
// (This symbol is defined in assembly, not here.)

/// Update the active young-space address range used by the write barrier.
///
/// This must be called by the GC during initialization and after each nursery
/// flip/resize that changes the current young generation region.
#[no_mangle]
pub extern "C" fn rt_gc_set_young_range(start: *mut u8, end: *mut u8) {
  abort_on_panic(|| {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_set_young_range();
    YOUNG_SPACE.start.store(start as usize, Ordering::Release);
    YOUNG_SPACE.end.store(end as usize, Ordering::Release);
  })
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
  abort_on_panic(|| unsafe {
    if !out_start.is_null() {
      *out_start = YOUNG_SPACE.start.load(Ordering::Acquire) as *mut u8;
    }
    if !out_end.is_null() {
      *out_end = YOUNG_SPACE.end.load(Ordering::Acquire) as *mut u8;
    }
  })
}

/// Set GC heap configuration for the process-global heap.
///
/// This must be called before the process-global heap is initialized (e.g. before the first
/// `rt_alloc` / `rt_gc_collect`). Returns false if the heap was already initialized.
#[no_mangle]
pub extern "C" fn rt_gc_set_config(cfg: *const crate::abi::RtGcConfig) -> bool {
  abort_on_panic(|| unsafe {
    if cfg.is_null() {
      trap::rt_trap_invalid_arg("rt_gc_set_config: cfg was null");
    }
    if (cfg as usize) % core::mem::align_of::<crate::abi::RtGcConfig>() != 0 {
      trap::rt_trap_invalid_arg("rt_gc_set_config: cfg was misaligned");
    }
    // Avoid reading the full `RtGcConfig` object by value: it contains padding bytes, and C callers
    // are not required to initialize padding. Read fields individually instead.
    let config = crate::gc::config::HeapConfig {
      nursery_size_bytes: core::ptr::addr_of!((*cfg).nursery_size_bytes).read(),
      los_threshold_bytes: core::ptr::addr_of!((*cfg).los_threshold_bytes).read(),
      // The stable ABI config does not expose major-GC mark parallelism tuning; default to the
      // runtime's auto-thread heuristic.
      major_gc_mark_threads: 0,
      minor_gc_nursery_used_percent: core::ptr::addr_of!((*cfg).minor_gc_nursery_used_percent).read(),
      major_gc_old_bytes_threshold: core::ptr::addr_of!((*cfg).major_gc_old_bytes_threshold).read(),
      major_gc_old_blocks_threshold: core::ptr::addr_of!((*cfg).major_gc_old_blocks_threshold).read(),
      major_gc_external_bytes_threshold: core::ptr::addr_of!((*cfg).major_gc_external_bytes_threshold).read(),
      promote_after_minor_survivals: core::ptr::addr_of!((*cfg).promote_after_minor_survivals).read(),
      // The public C ABI config does not currently expose mark-thread tuning. Use the runtime
      // default unless configured programmatically via `HeapConfig`.
      ..Default::default()
    };
    if let Err(msg) = config.validate() {
      trap::rt_trap_invalid_arg(msg);
    }
    crate::rt_alloc::try_set_global_heap_config(config)
  })
}

/// Set GC heap hard limits for the process-global heap.
///
/// This must be called before the process-global heap is initialized (e.g. before the first
/// `rt_alloc` / `rt_gc_collect`). Returns false if the heap was already initialized.
#[no_mangle]
pub extern "C" fn rt_gc_set_limits(limits: *const crate::abi::RtGcLimits) -> bool {
  abort_on_panic(|| unsafe {
    if limits.is_null() {
      trap::rt_trap_invalid_arg("rt_gc_set_limits: limits was null");
    }
    if (limits as usize) % core::mem::align_of::<crate::abi::RtGcLimits>() != 0 {
      trap::rt_trap_invalid_arg("rt_gc_set_limits: limits was misaligned");
    }
    let limits = crate::gc::config::HeapLimits {
      max_heap_bytes: core::ptr::addr_of!((*limits).max_heap_bytes).read(),
      max_total_bytes: core::ptr::addr_of!((*limits).max_total_bytes).read(),
    };
    if let Err(msg) = limits.validate() {
      trap::rt_trap_invalid_arg(msg);
    }
    crate::rt_alloc::try_set_global_heap_limits(limits)
  })
}

/// Debugging helper: snapshot the current GC heap configuration (or the pending config before
/// initialization).
#[no_mangle]
pub unsafe extern "C" fn rt_gc_get_config(out_cfg: *mut crate::abi::RtGcConfig) -> bool {
  abort_on_panic(|| unsafe {
    if out_cfg.is_null() {
      return false;
    }
    if (out_cfg as usize) % core::mem::align_of::<crate::abi::RtGcConfig>() != 0 {
      trap::rt_trap_invalid_arg("rt_gc_get_config: out_cfg was misaligned");
    }
    *out_cfg = crate::rt_alloc::global_heap_config_snapshot().to_rt();
    true
  })
}

/// Debugging helper: snapshot the current GC heap limits (or the pending limits before
/// initialization).
#[no_mangle]
pub unsafe extern "C" fn rt_gc_get_limits(out_limits: *mut crate::abi::RtGcLimits) -> bool {
  abort_on_panic(|| unsafe {
    if out_limits.is_null() {
      return false;
    }
    if (out_limits as usize) % core::mem::align_of::<crate::abi::RtGcLimits>() != 0 {
      trap::rt_trap_invalid_arg("rt_gc_get_limits: out_limits was misaligned");
    }
    *out_limits = crate::rt_alloc::global_heap_limits_snapshot().to_rt();
    true
  })
}

/// Reset write barrier state for tests.
///
/// This clears only process-global state used by the exported barrier:
/// - the active nursery range (`YOUNG_SPACE`), and
/// - the runtime's remembered-set tracking (used by GC model tests).
///
/// Per-object metadata (e.g. the `REMEMBERED` header bit) is owned by the
/// objects themselves and is not cleared (the runtime cannot enumerate all
/// objects in the current milestone GC).
#[doc(hidden)]
pub fn clear_write_barrier_state_for_tests() {
  // Keep integration tests isolated: the young-space range is global and affects
  // the write barrier's fast paths.
  rt_gc_set_young_range(core::ptr::null_mut(), core::ptr::null_mut());
  global_remset::remset_clear();
  // Tests that install a per-thread write-barrier context should not leak newly-remembered objects
  // across test cases.
  let thread = crate::mutator::current_mutator_thread_ptr();
  if !thread.is_null() {
    // Safety: tests install the pointer via `ThreadContextGuard`.
    unsafe { (*thread).new_remembered.clear() };
  }
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
  if (obj as usize) % crate::gc::OBJ_ALIGN != 0 {
    std::process::abort();
  }
  unsafe { (&*crate::gc::header_from_obj(obj)).is_remembered() }
}

/// Debug/test helper: rebuild remembered-set tracking after a simulated minor GC.
///
/// The exported write barrier sets the per-object `REMEMBERED` header bit and records newly
/// remembered objects in a fixed-capacity process-global list. Since the
/// milestone runtime cannot enumerate heap objects, tests that allocate/free synthetic objects can
/// leave stale pointers in that list.
///
/// This helper performs a **sticky rebuild** from `objs` (candidate old-generation objects):
/// - clears the process-global list
/// - for each object:
///   - if `object_has_young_refs(obj)` is `true`: set its `REMEMBERED` bit and add it to the list
///   - otherwise: clear its `REMEMBERED` bit
///
/// `objs` is supplied by tests because the milestone runtime cannot yet enumerate all heap objects.
#[doc(hidden)]
pub fn remembered_set_scan_and_rebuild_for_tests(
  objs: &[*mut u8],
  mut object_has_young_refs: impl FnMut(*mut u8) -> bool,
) {
  // Rebuild from the provided list of candidate old objects. `runtime-native` cannot currently
  // enumerate all objects from the heap, and the process-global remembered set may contain stale
  // pointers left behind by other tests.
  global_remset::remset_clear();
  for &obj in objs {
    if obj.is_null() {
      continue;
    }
    if (obj as usize) % crate::gc::OBJ_ALIGN != 0 {
      std::process::abort();
    }
    // SAFETY: alignment checked above; `obj` is expected to be a valid object base pointer.
    let header = unsafe { &*(obj as *const ObjHeader) };
    // Clear first so `remset_add` records the object even if it was already remembered.
    header.clear_remembered_idempotent();
    if object_has_young_refs(obj) {
      global_remset::remset_add(obj);
    }
  }
}

/// Returns the number of objects currently recorded in the global remembered set.
///
/// Intended for tests and debugging only.
#[doc(hidden)]
pub fn remembered_set_len_for_tests() -> usize {
  global_remset::remset_len_for_tests()
}
#[inline]
unsafe fn remember_old_object(obj: *mut u8) {
  if obj.is_null() {
    std::process::abort();
  }
  global_remset::remset_add(obj);
}
/// Write barrier for GC.
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier(obj: crate::roots::GcPtr, slot: *mut u8) {
  abort_on_panic(|| unsafe {
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
    if (obj as usize) % crate::gc::OBJ_ALIGN != 0 {
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

    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_write_barrier_old_young_hit();

    // Old → young store. Mark the base object as remembered.
    //
    // Note: `rt_write_barrier` is `NoGC` (must not allocate or safepoint). The process-global
    // remembered set is fixed-capacity; if it ever overflows we abort rather than silently drop an
    // old->young edge (which would be unsound).
    remember_old_object(obj);

    // If this object has a per-object card table, mark the card for the written slot.
    let header = &*crate::gc::header_from_obj(obj);
    let card_table = header.card_table_ptr();
    if !card_table.is_null() {
      let slot_offset = (slot as usize).wrapping_sub(obj as usize);
      let card = slot_offset / crate::gc::CARD_SIZE;
      mark_card_range(card_table, card, card);
    }
  })
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
  abort_on_panic(|| unsafe {
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_write_barrier_range();

    if obj.is_null() || start_slot.is_null() || len == 0 {
      return;
    }

    // Avoid UB on misaligned pointers: `obj` must be a valid `ObjHeader` base pointer.
    if (obj as usize) % crate::gc::OBJ_ALIGN != 0 {
      std::process::abort();
    }

    // Writes into young objects don't need a barrier: nursery tracing will find the edge.
    if YOUNG_SPACE.contains(obj as usize) {
      return;
    }

    // Old-object bulk write. Mark the base object as remembered (idempotently) so
    // minor GC can consult its dirty cards and/or rescan it.
    remember_old_object(obj);

    let header = &*crate::gc::header_from_obj(obj);
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
  })
}

#[cfg(test)]
mod write_barrier_tests {
  use super::*;
  use crate::test_util::TestGcGuard;
  fn with_test_lock<T>(f: impl FnOnce() -> T) -> T {
    // Serialize with integration tests that also mutate global write barrier
    // state (young range + remembered set).
    let _g = TestGcGuard::new();
    f()
  }

  #[repr(C)]
  #[repr(align(16))]
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

      let obj_ptr = crate::gc::obj_from_header((&mut old.header) as *mut ObjHeader);
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

      let obj_ptr = crate::gc::obj_from_header((&mut old.header) as *mut ObjHeader);
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
/// This entrypoint coordinates a cooperative stop-the-world (STW) safepoint across registered
/// threads, enumerates all GC roots (stackmap roots + registered root slots), and runs a full heap
/// collection (minor nursery evacuation + major mark/sweep).
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_gc_collect() {
  // Capture the current frame pointer *before* entering `abort_on_panic` (which
  // uses `std::panic::catch_unwind` in `panic=unwind` builds).
  //
  // `std::panic::catch_unwind` lives in the prebuilt sysroot and may not preserve a
  // reliable frame-pointer chain (e.g. it can repurpose RBP as a general register).
  // If we call `find_nearest_managed_cursor_from_here` from inside the `catch_unwind`
  // closure, the saved "caller FP" in that closure frame may be junk and the walk
  // will fail, causing us to publish a runtime-internal IP instead of the managed
  // safepoint callsite.
  //
  // By grabbing the FP of this outer `rt_gc_collect` frame here and walking from it
  // later, we avoid depending on the sysroot's FP behavior while still keeping the
  // extern "C" boundary abort-on-panic.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  let fallback_ctx = crate::arch::capture_safepoint_context();

  abort_on_panic(|| rt_gc_collect_impl(GcCollectKind::Major, "rt_gc_collect", entry_fp, fallback_ctx))
}

/// Trigger a stop-the-world minor GC cycle (nursery evacuation).
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_gc_collect_minor() {
  // See `rt_gc_collect` for detailed commentary on why we capture these values
  // outside `abort_on_panic`.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  let fallback_ctx = crate::arch::capture_safepoint_context();
  abort_on_panic(|| rt_gc_collect_impl(GcCollectKind::Minor, "rt_gc_collect_minor", entry_fp, fallback_ctx))
}

/// Trigger a stop-the-world major GC cycle (full heap collection).
///
/// This is a stable ABI alias for [`rt_gc_collect`].
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_gc_collect_major() {
  // See `rt_gc_collect` for detailed commentary on why we capture these values
  // outside `abort_on_panic`.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  let fallback_ctx = crate::arch::capture_safepoint_context();
  abort_on_panic(|| rt_gc_collect_impl(GcCollectKind::Major, "rt_gc_collect_major", entry_fp, fallback_ctx))
}

#[inline]
fn safepoint_context_from_entry_fp(entry_fp: u64) -> crate::arch::SafepointContext {
  if entry_fp == 0 {
    return crate::arch::SafepointContext::default();
  }

  // SAFETY: `entry_fp` was captured from a runtime entrypoint frame compiled with frame pointers.
  // Under the forced-frame-pointer ABI:
  // - [entry_fp + 0] is the caller's frame pointer (managed frame)
  // - [entry_fp + WORD_SIZE] is the return address into the managed frame (callsite PC)
  let outer_fp = unsafe { (entry_fp as *const u64).read() };
  let outer_ip = unsafe { ((entry_fp + crate::arch::WORD_SIZE as u64) as *const u64).read() };

  // Caller SP at the callsite into the runtime entrypoint is `entry_fp + 2 * WORD_SIZE` for both
  // x86_64 and aarch64 under forced frame pointers.
  let sp_callsite = entry_fp.saturating_add((crate::arch::WORD_SIZE * 2) as u64);
  #[cfg(target_arch = "x86_64")]
  let sp_entry = sp_callsite.saturating_sub(crate::arch::WORD_SIZE as u64);
  #[cfg(not(target_arch = "x86_64"))]
  let sp_entry = sp_callsite;

  crate::arch::SafepointContext {
    sp_entry: sp_entry as usize,
    sp: sp_callsite as usize,
    fp: outer_fp as usize,
    ip: outer_ip as usize,
    regs: core::ptr::null_mut(),
  }
}

pub(crate) fn gc_collect_minor_for_alloc(entry_name: &'static str, entry_fp: u64) {
  // Do not wrap this helper in `abort_on_panic`: it is always called from within a runtime
  // entrypoint that already aborts on panic, and avoiding an extra `catch_unwind` layer keeps the
  // frame-pointer chain usable for stackmap fixups.
  let fallback_ctx = safepoint_context_from_entry_fp(entry_fp);
  rt_gc_collect_impl(GcCollectKind::Minor, entry_name, entry_fp, fallback_ctx)
}

pub(crate) fn gc_collect_major_for_alloc(entry_name: &'static str, entry_fp: u64) {
  let fallback_ctx = safepoint_context_from_entry_fp(entry_fp);
  rt_gc_collect_impl(GcCollectKind::Major, entry_name, entry_fp, fallback_ctx)
}

fn rt_gc_collect_impl(
  kind: GcCollectKind,
  entry_name: &'static str,
  entry_fp: u64,
  fallback_ctx: crate::arch::SafepointContext,
) {
  fn publish_current_thread_safepoint_context(
    entry_fp: u64,
    fallback_ctx: crate::arch::SafepointContext,
    stop_epoch: u64,
  ) {
    // If the current thread isn't registered, it doesn't participate in stackmap root enumeration.
    if registry::current_thread_id().is_none() {
      return;
    }

    // If GC is triggered while executing inside runtime code (e.g. `rt_alloc` decides to collect),
    // the nearest managed frame is suspended at the callsite into the *outermost* runtime frame
    // (which has a stackmap record). Recover that managed callsite cursor by walking the FP chain.
    //
    // Call `capture_safepoint_context` directly from `rt_gc_collect*` on fallback.
    //
    // The capture helper walks the frame-pointer chain assuming it is invoked by a *runtime helper*
    // frame. Calling it indirectly (e.g. via `Option::unwrap_or_else`) introduces an extra frame and
    // changes which FP/IP are captured.
    let ctx = match crate::stackmap::try_stackmaps()
      .and_then(|stackmaps| crate::stackwalk::find_nearest_managed_cursor(entry_fp, stackmaps))
    {
      Some(cursor) => {
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
          regs: core::ptr::null_mut(),
        }
      }
      None => fallback_ctx,
    };

    registry::set_current_thread_safepoint_context(ctx);
    registry::set_current_thread_safepoint_epoch_observed(stop_epoch);
    crate::threading::safepoint::notify_state_change();
  }

  fn join_stop_the_world(entry_fp: u64, fallback_ctx: crate::arch::SafepointContext, stop_epoch: u64) {
    publish_current_thread_safepoint_context(entry_fp, fallback_ctx, stop_epoch);

    // Block until the stop-the-world epoch is resumed.
    crate::threading::safepoint::wait_while_stop_the_world();

    // Post-resume barrier: publish that we've observed the resumed (even) epoch before returning to
    // mutator code. Without this, the coordinator's `rt_gc_wait_for_world_resumed_timeout` can time
    // out if this thread joined the stop-the-world request via `rt_gc_collect*` instead of the
    // standard `rt_gc_safepoint` slow path.
    if registry::current_thread_id().is_some() {
      let resume_epoch = crate::threading::safepoint::current_epoch();
      registry::set_current_thread_safepoint_epoch_observed(resume_epoch);
      crate::threading::safepoint::notify_state_change();
    }
  }

  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_gc_collect();

  // Tests (and some embedders) may reset the exported young range between runs.
  // Ensure it always reflects the nursery backing the process-global heap before
  // starting a collection.
  //
  // This must remain allocation-free after `rt_thread_init` (see `tests/no_alloc_rt_gc_collect.rs`).
  crate::rt_alloc::ensure_global_heap_init();

  // Any minor GC increments the nursery epoch, and any major GC increments the
  // major epoch. Use these as the "satisfaction" counters so:
  // - concurrent `rt_gc_collect_minor` calls coalesce with each other or with a
  //   major collection, and
  // - `rt_gc_collect` / `rt_gc_collect_major` are not incorrectly satisfied by a
  //   concurrent minor collection or unrelated stop-the-world phase.
  let satisfied_epoch_before = kind.satisfied_epoch();

  loop {
    // If a stop-the-world is already active, join it at this callsite (so we still
    // publish a safepoint context for stack walking). Then check whether it
    // satisfied the requested kind; if not, loop and attempt to initiate the
    // requested collection after the world resumes.
    let epoch_before = crate::threading::safepoint::current_epoch();
    if epoch_before & 1 == 1 {
      join_stop_the_world(entry_fp, fallback_ctx, epoch_before);
      if kind.satisfied_epoch() != satisfied_epoch_before {
        return;
      }
      continue;
    }

    let _gc_collect_guard = GC_COLLECT_MUTEX.lock();

    // If a satisfying collection completed while we were waiting on the collector lock,
    // do not start a second stop-the-world cycle. Instead, publish that we've observed
    // the current (even) safepoint epoch and return.
    if kind.satisfied_epoch() != satisfied_epoch_before {
      let epoch = crate::threading::safepoint::current_epoch();
      if epoch & 1 == 1 {
        // Stop-the-world is currently active (e.g. initiated via another API);
        // release the lock and join it.
        drop(_gc_collect_guard);
        join_stop_the_world(entry_fp, fallback_ctx, epoch);
      } else if registry::current_thread_id().is_some() {
        registry::set_current_thread_safepoint_epoch_observed(epoch);
        crate::threading::safepoint::notify_state_change();
      }
      return;
    }

    // A stop-the-world request may have started while we were waiting on the collector lock.
    // Join it, then re-check satisfaction.
    let epoch_after_lock = crate::threading::safepoint::current_epoch();
    if epoch_after_lock & 1 == 1 {
      drop(_gc_collect_guard);
      join_stop_the_world(entry_fp, fallback_ctx, epoch_after_lock);
      if kind.satisfied_epoch() != satisfied_epoch_before {
        return;
      }
      continue;
    }

    // Attempt to become the stop-the-world coordinator.
    let Some(stop_epoch) = crate::threading::safepoint::rt_gc_try_request_stop_the_world() else {
      // Another stop-the-world request beat us (could be GC or another STW
      // protocol). Join it if active; otherwise, loop and re-check whether the
      // requested kind was satisfied.
      let epoch = crate::threading::safepoint::current_epoch();
      drop(_gc_collect_guard);
      if epoch & 1 == 1 {
        join_stop_the_world(entry_fp, fallback_ctx, epoch);
      }
      if kind.satisfied_epoch() != satisfied_epoch_before {
        return;
      }
      continue;
    };

    // If this entrypoint is called from an attached mutator thread, publish the
    // initiator's safepoint context before waiting for other threads. This keeps
    // the initiator's stack eligible for stackmap-based root enumeration while
    // the world is stopped.
    publish_current_thread_safepoint_context(entry_fp, fallback_ctx, stop_epoch);

    crate::safepoint::with_world_stopped_requested(stop_epoch, move || {
      struct AbiRootSet {
        stop_epoch: u64,
        entry_name: &'static str,
      }

      impl crate::gc::RootSet for AbiRootSet {
        fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
          // Roots for stopped mutator threads + global roots/handles.
          crate::threading::safepoint::for_each_root_slot_world_stopped(self.stop_epoch, |slot| {
            f(slot);
          })
          .unwrap_or_else(|err| {
            eprintln!("{}: failed to enumerate roots: {err:?}", self.entry_name);
            std::process::abort();
          });
        }
      }

      let mut remembered = global_remset::WorldStoppedRememberedSet::new();
      let mut roots = AbiRootSet { stop_epoch, entry_name };

      let res = crate::rt_alloc::with_heap_lock_world_stopped(|heap| match kind {
        GcCollectKind::Minor => heap.collect_minor(&mut roots, &mut remembered),
        GcCollectKind::Major => heap.collect_major(&mut roots, &mut remembered),
      });
      if res.is_err() {
        crate::trap::rt_trap_oom(0, kind.oom_trap_msg(entry_name));
      }
    });

    return;
  }
}

/// Returns the total number of bytes currently held in non-moving backing stores (e.g. `ArrayBuffer`
/// bytes) allocated outside the GC heap.
///
/// This value is intended for memory-pressure heuristics: large external buffers should contribute
/// to GC trigger decisions even though they are not part of the moving heap.
#[no_mangle]
pub extern "C" fn rt_backing_store_external_bytes() -> usize {
  abort_on_panic(|| crate::buffer::backing_store::global_backing_store_allocator().external_bytes())
}

/// Test/debug helper: return the total number of externally allocated bytes tracked by the
/// process-global GC heap.
///
/// This includes:
/// - backing store allocations (ArrayBuffer, etc), and
/// - other external allocations tracked directly by the GC heap (e.g. payload-promise buffers).
#[doc(hidden)]
pub fn rt_debug_heap_external_bytes() -> usize {
  crate::rt_alloc::with_heap_lock_mutator(|heap| heap.external_bytes())
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
  abort_on_panic(|| {
    if slot.is_null() {
      crate::trap::rt_trap_invalid_arg("rt_root_push: slot was null");
    }
    let Some(thread) = registry::current_thread_state() else {
      crate::trap::rt_trap_invalid_arg("rt_root_push: current thread is not registered");
    };
    thread.handle_stack_push(slot);
  })
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
  abort_on_panic(|| {
    if slot.is_null() {
      crate::trap::rt_trap_invalid_arg("rt_root_pop: slot was null");
    }
    let Some(thread) = registry::current_thread_state() else {
      crate::trap::rt_trap_invalid_arg("rt_root_pop: current thread is not registered");
    };
    thread.handle_stack_pop_debug(slot);
  })
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
  abort_on_panic(|| crate::roots::register_global_root_slot(slot))
}

/// Unregister a global/static root slot previously registered via [`rt_global_root_register`].
#[no_mangle]
pub extern "C" fn rt_global_root_unregister(slot: *mut usize) {
  abort_on_panic(|| crate::roots::unregister_global_root_slot(slot))
}

/// Register an addressable root slot with the runtime.
///
/// `slot` must point to a writable `*mut u8` and must remain valid until the
/// returned handle is passed to [`rt_gc_unregister_root_slot`].
#[no_mangle]
pub extern "C" fn rt_gc_register_root_slot(slot: crate::roots::GcHandle) -> u32 {
  abort_on_panic(|| crate::roots::global_root_registry().register_root_slot(slot))
}

/// Unregister a previously registered root slot handle.
#[no_mangle]
pub extern "C" fn rt_gc_unregister_root_slot(handle: u32) {
  abort_on_panic(|| {
    crate::roots::global_root_registry().unregister(handle);
  })
}

/// Convenience API: create an internal root slot initialized to `ptr`.
///
/// This is primarily intended for FFI/host embeddings that want a persistent
/// handle without managing slot storage themselves.
///
/// ## Moving-GC safety
/// This function accepts a raw pointer value. If `ptr` is a **movable** GC-managed pointer, prefer
/// [`rt_gc_pin_h`] so the runtime reads the pointer from an addressable slot after acquiring the
/// root registry lock (lock contention may temporarily enter a GC-safe region, allowing a moving GC
/// to relocate objects).
#[no_mangle]
pub extern "C" fn rt_gc_pin(ptr: crate::roots::GcPtr) -> u32 {
  abort_on_panic(|| crate::roots::global_root_registry().pin(ptr))
}

/// Like [`rt_gc_pin`], but takes the pointer as a `GcHandle` (pointer-to-slot) handle.
///
/// This is the moving-GC-safe variant: the runtime will only read the pointer value from `slot`
/// *after* acquiring the root registry lock, so a moving GC can update the slot if lock acquisition
/// blocks.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing an object base
/// pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_gc_pin_h(slot: crate::roots::GcHandle) -> u32 {
  abort_on_panic(|| unsafe { crate::roots::global_root_registry().pin_from_slot(slot) })
}

/// Destroy a handle created by [`rt_gc_pin`].
#[no_mangle]
pub extern "C" fn rt_gc_unpin(handle: u32) {
  abort_on_panic(|| {
    crate::roots::global_root_registry().unregister(handle);
  })
}

/// Returns the current pointer value for a root handle created by
/// [`rt_gc_register_root_slot`] or [`rt_gc_pin`].
///
/// Returns null if `handle` is invalid/stale/removed.
#[no_mangle]
pub extern "C" fn rt_gc_root_get(handle: u32) -> *mut u8 {
  abort_on_panic(|| {
    crate::roots::global_root_registry()
      .get(handle)
      .unwrap_or(std::ptr::null_mut())
  })
}

/// Updates the pointer value for a root handle created by [`rt_gc_register_root_slot`] or
/// [`rt_gc_pin`].
///
/// Returns `false` if `handle` is invalid/stale/removed.
///
/// ## Moving-GC safety
/// This function accepts a raw pointer value. If `ptr` is a **movable** GC-managed pointer, prefer
/// [`rt_gc_root_set_h`] so the runtime reads the pointer from an addressable slot after acquiring
/// the root registry lock.
#[no_mangle]
pub extern "C" fn rt_gc_root_set(handle: u32, ptr: *mut u8) -> bool {
  abort_on_panic(|| crate::roots::global_root_registry().set(handle, ptr))
}

/// Like [`rt_gc_root_set`], but takes the new pointer value as a `GcHandle` (pointer-to-slot)
/// handle.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing an object base
/// pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_gc_root_set_h(handle: u32, slot: crate::roots::GcHandle) -> bool {
  abort_on_panic(|| unsafe { crate::roots::global_root_registry().set_from_slot(handle, slot) })
}

// -----------------------------------------------------------------------------
// Persistent handle IDs (stable u64)
// -----------------------------------------------------------------------------
//
// These are stable integer IDs intended for crossing async / OS / thread boundaries (epoll/kqueue
// userdata, cross-thread wakeups, ...). They are backed by the process-global persistent handle
// table (`roots::PersistentHandleTable`) so the GC can update the stored pointer when objects move.

/// Allocate a new persistent handle rooting `ptr`.
///
/// ## Moving-GC safety
/// This function accepts a raw pointer value. It is therefore only safe to use with:
/// - pointers that are **not** GC-managed, or
/// - GC-managed pointers that are known to be **stable** for the duration of the call (e.g. pinned
///   objects).
///
/// If `ptr` refers to a **movable** GC-managed object, use [`rt_handle_alloc_h`] instead so the
/// runtime can re-load the pointer from an addressable slot *after* acquiring internal locks (lock
/// contention may temporarily enter a GC-safe region, allowing a moving GC to relocate objects).
#[no_mangle]
pub extern "C" fn rt_handle_alloc(ptr: *mut u8) -> u64 {
  abort_on_panic(|| crate::roots::global_persistent_handle_table().alloc(ptr).to_u64())
}

/// Like [`rt_handle_alloc`], but takes the GC-managed pointer as a `GcHandle` (pointer-to-slot)
/// handle.
///
/// This is the moving-GC-safe variant: the runtime will only read the pointer value from `slot`
/// *after* acquiring the persistent handle table lock, so a GC can update the slot if lock
/// acquisition blocks.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_handle_alloc_h(slot: crate::roots::GcHandle) -> u64 {
  abort_on_panic(|| unsafe {
    crate::roots::global_persistent_handle_table()
      .alloc_from_slot(slot)
      .to_u64()
  })
}

/// Free a persistent handle created by [`rt_handle_alloc`].
///
/// Invalid handles are ignored.
#[no_mangle]
pub extern "C" fn rt_handle_free(handle: u64) {
  abort_on_panic(|| {
    let _ = crate::roots::global_persistent_handle_table().free(HandleId::from_u64(handle));
  })
}

/// Resolve a persistent handle back to the (possibly relocated) pointer stored in its slot.
///
/// Returns null if the handle is invalid or has been freed.
#[no_mangle]
pub extern "C" fn rt_handle_load(handle: u64) -> *mut u8 {
  abort_on_panic(|| {
    crate::roots::global_persistent_handle_table()
      .get(HandleId::from_u64(handle))
      .unwrap_or(std::ptr::null_mut())
  })
}

/// Update the pointer stored in a persistent handle slot.
///
/// Invalid handles are ignored.
///
/// ## Moving-GC safety
/// This function accepts a raw pointer value. If the new value is a **movable** GC-managed object,
/// prefer [`rt_handle_store_h`] so the runtime reads the pointer from a slot after acquiring the
/// handle table lock.
#[no_mangle]
pub extern "C" fn rt_handle_store(handle: u64, ptr: *mut u8) {
  abort_on_panic(|| {
    let _ = crate::roots::global_persistent_handle_table().set(HandleId::from_u64(handle), ptr);
  })
}

/// Like [`rt_handle_store`], but takes the new pointer value as a `GcHandle` (pointer-to-slot)
/// handle.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_handle_store_h(handle: u64, slot: crate::roots::GcHandle) {
  abort_on_panic(|| unsafe {
    let _ = crate::roots::global_persistent_handle_table()
      .set_from_slot(HandleId::from_u64(handle), slot);
  })
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_stats_snapshot(out: *mut RtGcStatsSnapshot) {
  abort_on_panic(|| unsafe {
    if out.is_null() {
      return;
    }
    *out = crate::gc_stats::snapshot();
  })
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub extern "C" fn rt_gc_stats_reset() {
  abort_on_panic(|| {
    crate::gc_stats::reset();
  })
}

// -----------------------------------------------------------------------------
// Weak handles (non-owning references)
// -----------------------------------------------------------------------------

/// Create a new weak handle for `value`.
///
/// Weak handles do not keep the referent alive. If the referent is collected, `rt_weak_get`
/// returns null.
///
/// ## Moving-GC safety
/// If `value` is a **movable** GC-managed pointer, prefer [`rt_weak_add_h`]. Lock contention while
/// registering the weak handle may temporarily enter a GC-safe region, allowing a moving GC to
/// relocate objects; `rt_weak_add_h` reads the pointer from an addressable slot after acquiring the
/// weak-handle table lock.
#[no_mangle]
pub extern "C" fn rt_weak_add(value: crate::roots::GcPtr) -> u64 {
  abort_on_panic(|| crate::gc::weak::global_weak_add(value).as_u64())
}

/// Like [`rt_weak_add`], but takes the referent as a `GcHandle` (pointer-to-slot) handle.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_weak_add_h(slot: crate::roots::GcHandle) -> u64 {
  abort_on_panic(|| unsafe { crate::gc::weak::global_weak_add_from_slot(slot).as_u64() })
}

/// Resolve a weak handle back to a pointer, or null if the referent is dead/cleared.
#[no_mangle]
pub extern "C" fn rt_weak_get(handle: u64) -> crate::roots::GcPtr {
  abort_on_panic(|| {
    crate::gc::weak::global_weak_get(WeakHandle::from_u64(handle)).unwrap_or(std::ptr::null_mut())
  })
}

/// Remove a weak handle.
#[no_mangle]
pub extern "C" fn rt_weak_remove(handle: u64) {
  abort_on_panic(|| {
    crate::gc::weak::global_weak_remove(WeakHandle::from_u64(handle));
  })
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().spawn(task, data)
  })
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn_rooted(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().spawn_rooted(task, data)
  })
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn_promise_legacy(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    // Ensure the async runtime is initialized so promise settlement can wake a blocked `epoll_wait`.
    let _ = async_rt::global();

    let promise = async_rt::promise::promise_new();
    if !promise.is_null() {
      let header = promise.0.cast::<PromiseHeader>();
      if header.is_null() {
        std::process::abort();
      }
      unsafe {
        (*header).flags.fetch_or(crate::async_abi::PROMISE_FLAG_EXTERNAL_PENDING, Ordering::Release);
      }
      async_rt::external_pending_inc();
    }

    #[repr(C)]
    struct WorkItem {
      task: extern "C" fn(*mut u8, PromiseRef),
      data: *mut u8,
      promise: PromiseRef,
    }

    // Raw pointers are not `Send` by default; in the runtime ABI the caller is responsible for
    // ensuring `data` is valid to access from a parallel worker thread.
    unsafe impl Send for WorkItem {}

    extern "C" fn run_work_item(data: *mut u8) {
      // Safety: allocated by `Box::into_raw` below.
      let work = unsafe { Box::from_raw(data as *mut WorkItem) };
      // `work.task` is typed as `extern "C"`. If it panics we must not unwind across the FFI
      // boundary (UB); abort deterministically instead.
      crate::ffi::invoke_cb2_promise(work.task, work.data, work.promise);
    }

    let work = Box::new(WorkItem { task, data, promise });
    crate::rt_parallel().spawn_detached(run_work_item, Box::into_raw(work) as *mut u8);
    promise
  })
}

#[no_mangle]
pub unsafe extern "C" fn rt_parallel_spawn_rooted_h(
  task: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
) -> TaskId {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    // Safety: caller promises `data` points at a valid `*mut u8` GC slot.
    unsafe { crate::rt_parallel().spawn_rooted_h(task, data) }
  })
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(tasks: *const TaskId, count: usize) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().join(tasks, count)
  })
}

#[no_mangle]
pub extern "C" fn rt_parallel_for(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().parallel_for(start, end, body, data)
  })
}

/// Like [`rt_parallel_for`], but treats `data` as a GC-managed object that the runtime will keep
/// alive (and relocatable) for the duration of the call.
///
/// This is required when `body` captures a GC-managed environment object and the runtime chooses to
/// parallelize the loop: the userdata pointer is stored in Rust-owned scheduler state that is not
/// visible to stackmap-based GC scanning.
///
/// Contract:
/// - `data` must be a pointer to the base of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until the `rt_parallel_for_rooted` call
///   returns.
/// - The `body` callback receives the current relocated pointer after any GC relocation.
#[no_mangle]
pub extern "C" fn rt_parallel_for_rooted(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    crate::rt_parallel().parallel_for_rooted(start, end, body, data)
  })
}

/// Like [`rt_parallel_for_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_parallel_for_rooted_h(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: crate::roots::GcHandle,
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    // Safety: caller contract.
    unsafe { crate::rt_parallel().parallel_for_rooted_h(start, end, body, data) }
  })
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
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::parallel_integration::spawn_promise(task, data, promise_layout)
  })
}

/// Like [`rt_parallel_spawn_promise`], but `data` is a GC-managed object that the runtime will keep
/// alive until the worker task finishes executing.
///
/// Contract:
/// - `data` must be a pointer to the base of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until the task completes.
/// - The worker callback receives the (possibly relocated) pointer after any GC relocation.
#[no_mangle]
pub extern "C" fn rt_parallel_spawn_promise_rooted(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_layout: PromiseLayout,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::parallel_integration::spawn_promise_rooted(task, data, promise_layout)
  })
}

/// Like [`rt_parallel_spawn_promise_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_parallel_spawn_promise_rooted_h(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: crate::roots::GcHandle,
  promise_layout: PromiseLayout,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    // Safety: caller contract.
    unsafe { crate::parallel_integration::spawn_promise_rooted_h(task, data, promise_layout) }
  })
}

/// Like [`rt_parallel_spawn_promise`], but allocates the promise as a **GC-managed object** with the
/// provided `promise_shape`.
///
/// This is required when the promise payload contains GC pointers: the runtime uses `promise_shape`
/// to precisely trace and update those pointers during moving GC.
///
/// The promise payload begins immediately after the [`PromiseHeader`] prefix (same as the native
/// async ABI). Use [`rt_promise_payload_ptr`] to obtain the payload pointer.
#[no_mangle]
pub extern "C" fn rt_parallel_spawn_promise_with_shape(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::parallel_integration::spawn_promise_with_shape(
      task,
      data,
      promise_size,
      promise_align,
      promise_shape,
    )
  })
}

/// Like [`rt_parallel_spawn_promise_with_shape`], but `data` is a GC-managed object that the runtime
/// will keep alive until the worker task finishes executing.
///
/// Contract:
/// - `data` must be a pointer to the base of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until the task completes.
/// - The worker callback receives the (possibly relocated) pointer after any GC relocation.
#[no_mangle]
pub extern "C" fn rt_parallel_spawn_promise_with_shape_rooted(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::parallel_integration::spawn_promise_with_shape_rooted(
      task,
      data,
      promise_size,
      promise_align,
      promise_shape,
    )
  })
}

/// Like [`rt_parallel_spawn_promise_with_shape_rooted`], but takes the GC-managed `data` pointer as a
/// `GcHandle` (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_parallel_spawn_promise_with_shape_rooted_h(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: crate::roots::GcHandle,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    // Safety: caller contract.
    unsafe {
      crate::parallel_integration::spawn_promise_with_shape_rooted_h(
        task,
        data,
        promise_size,
        promise_align,
        promise_shape,
      )
    }
  })
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
    ensure_event_loop_thread_registered();
    async_rt::coroutine::async_spawn_deferred(coro)
  })
}

/// Tear down all pending async work without running it.
///
/// This is intended for embedders (and generated native programs) that need to abandon the
/// event-loop early (termination, timeouts, shutdown) but still want to release any resources/GC
/// roots held by queued jobs.
#[no_mangle]
pub extern "C" fn rt_async_cancel_all() {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    // Treat cancellation as a "driving" operation: it destroys coroutine frames that may otherwise
    // be resumed by the event loop.
    let _ = async_rt::with_driver_guard("rt_async_cancel_all", || {
      // Cancel runtime-owned coroutine frames for the native async ABI (`async_abi`).
      crate::async_runtime::cancel_all();

      // Cancel all legacy executor work (microtasks/macrotasks/timers/I/O watchers).
      async_rt::cancel_all_pending_work_under_driver_guard();

      // Drop pending promise reactions stored on unresolved promises (otherwise those reactions can
      // keep awaiting coroutines alive indefinitely after shutdown).
      async_rt::promise::cancel_all_pending_reactions();

      // Clear any outstanding unhandled rejection tracker state so we don't retain promises as
      // roots after teardown.
      crate::unhandled_rejection::clear_state();

      // Clear the web timer bookkeeping map so subsequent timer operations don't observe stale
      // entries (the underlying async runtime timers have been cancelled above).
      clear_web_timers();

      // Clear runaway error state / reentrancy guard so the runtime can be reused after teardown.
      crate::async_runtime::reset_after_cancel();
    });
  })
}

/// Drive the runtime's async/event-loop queues.
///
/// The async runtime is single-driver: only one thread may execute the poll loop at a time.
///
/// - Concurrent driving from another thread aborts (fail-fast).
/// - Re-entrant calls on the same thread are treated as a no-op and return `false`.
///
/// Returns `true` if there is still pending work after this poll turn (queued tasks, active
/// timers, I/O watchers, or outstanding external work). Returns `false` when the runtime is
/// quiescent.
///
/// Note: This is a compatibility alias for [`crate::rt_async_poll`].
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
    if async_runtime::has_error() {
      return;
    }
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

    if async_runtime::has_error() {
      return;
    }

    // `rt_async_block_on` is itself a driving entrypoint: hold the driver guard for the entire
    // blocking loop so other threads cannot concurrently call into `rt_async_poll` /
    // `rt_async_run_until_idle` / etc. Same-thread re-entrancy becomes a no-op via
    // `with_driver_guard`.
    let _ = async_rt::with_driver_guard("rt_async_block_on", || {
      // Fast path: already settled.
      if !promise_is_pending(p) {
        return;
      }

      // Ensure the event loop is woken when `p` is settled even if nothing else is
      // awaiting it. Without this, `rt_async_wait` can sleep indefinitely.
      register_block_on_waker(p);

      loop {
        let _ = crate::async_runtime::rt_async_run_until_idle_under_driver_guard();

        if !promise_is_pending(p) {
          return;
        }

        if async_runtime::has_error() {
          return;
        }

        // No ready work; park until something wakes the runtime.
        async_rt::wait_for_work_under_driver_guard();
      }
    });
  })
}

// -----------------------------------------------------------------------------
// Microtasks (queueMicrotask-style jobs)
// -----------------------------------------------------------------------------

#[repr(C)]
struct MicrotaskWithDrop {
  func: extern "C" fn(*mut u8),
  data: *mut u8,
  drop: extern "C" fn(*mut u8),
  ran: bool,
}

extern "C" fn run_microtask_with_drop(data: *mut u8) {
  // Safety: allocated by `Box::into_raw(MicrotaskWithDrop)` in the queueing helpers below and freed
  // by `drop_microtask_with_drop`.
  let task = unsafe { &mut *(data as *mut MicrotaskWithDrop) };
  task.ran = true;
  crate::ffi::invoke_cb1(task.func, task.data);
}

extern "C" fn drop_microtask_with_drop(data: *mut u8) {
  // Safety: allocated by `Box::into_raw(MicrotaskWithDrop)` in the queueing helpers below.
  let task = unsafe { Box::from_raw(data as *mut MicrotaskWithDrop) };
  if !task.ran {
    crate::ffi::invoke_cb1(task.drop, task.data);
  }
}

fn enqueue_microtask_with_optional_drop(
  func: extern "C" fn(*mut u8),
  data: *mut u8,
  drop: Option<extern "C" fn(*mut u8)>,
) {
  match drop {
    None => async_rt::global().enqueue_microtask(async_rt::Task::new(func, data)),
    Some(drop) => {
      let task = Box::new(MicrotaskWithDrop {
        func,
        data,
        drop,
        ran: false,
      });
      async_rt::global().enqueue_microtask(async_rt::Task::new_with_drop(
        run_microtask_with_drop,
        Box::into_raw(task) as *mut u8,
        drop_microtask_with_drop,
      ));
    }
  }
}

/// Enqueue a single microtask callback onto the async runtime's microtask queue.
///
/// This is a low-level primitive that can be used to implement Web-standard
/// `queueMicrotask(cb)` without allocating a promise/coroutine frame.
///
/// # Safety
/// - `task.func` must be a valid function pointer.
/// - `task.data` must remain valid until the callback runs (or until `task.drop` is called).
/// - If the microtask is discarded without running (e.g. `rt_async_cancel_all`),
///   `task.drop(task.data)` is called if `task.drop` is non-null.
#[no_mangle]
pub unsafe extern "C" fn rt_queue_microtask(task: Microtask) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();

    // Function pointers are non-null by construction in Rust, but this is an
    // FFI-exposed ABI type and may be constructed from foreign code.
    if (task.func as usize) == 0 {
      std::process::abort();
    }

    enqueue_microtask_with_optional_drop(task.func, task.data, task.drop);
  })
}

/// Drain only the microtask queue.
///
/// Unlike [`crate::rt_async_poll`] / [`rt_async_poll_legacy`], this does *not*
/// run macrotasks, timers, or reactor callbacks.
///
/// Returns `true` if any microtasks were executed.
///
/// Note: This is implemented as a thin C-ABI wrapper around
/// [`crate::rt_drain_microtasks`] (the Rust API), which provides non-reentrant
/// "microtask checkpoint" semantics.
#[export_name = "rt_drain_microtasks"]
pub extern "C" fn rt_drain_microtasks_abi() -> bool {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::async_runtime::rt_drain_microtasks()
  })
}

#[inline]
fn rt_async_sleep_impl(delay_ms: u64) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();

  // Allocate a minimal native async-ABI promise in the GC heap.
  //
  // This must *not* use the legacy `async_rt::promise::promise_new()` allocator (`Box<RtPromise>`)
  // because `rt_async_sleep` is a stable runtime API and is expected to be usable in long-running
  // programs without leaking Rust heap allocations.
  static ASYNC_SLEEP_PROMISE_PTR_OFFSETS: [u32; 0] = [];
  static ASYNC_SLEEP_PROMISE_TYPE_DESC: TypeDescriptor = TypeDescriptor::new(
    core::mem::size_of::<PromiseHeader>(),
    &ASYNC_SLEEP_PROMISE_PTR_OFFSETS,
  );

  let obj = crate::rt_alloc::alloc_typed(&ASYNC_SLEEP_PROMISE_TYPE_DESC);
  let promise = PromiseRef(obj.cast());
  unsafe {
    // Initialize the `PromiseHeader` atomics without creating references to uninitialized `Atomic*`
    // values (see `native_async::promise_init` safety notes).
    crate::native_async::promise_init(promise);
  }

  extern "C" fn fulfill_sleep(data: *mut u8) {
    let promise = PromiseRef(data.cast());
    unsafe {
      crate::native_async::promise_fulfill(promise);
    }
  }

  // Timers store callback userdata in Rust-owned queues that are not scanned by the GC. Keep the
  // promise alive (and relocatable) via a persistent handle root until the timer fires.
  let task = {
    // Root the freshly-allocated promise via the shadow stack while we create the persistent handle
    // for the queued task. This avoids a moving-GC TOCTOU where contended lock acquisition could
    // safepoint before the persistent handle is installed.
    let tmp_root = crate::roots::Root::<u8>::new(obj);
    // Safety: `tmp_root.handle()` points at a valid `*mut u8` GC pointer slot.
    unsafe { async_rt::Task::new_gc_rooted_h(fulfill_sleep, tmp_root.handle()) }
  };

  let _timer_id =
    async_rt::global().schedule_timer_in(Duration::from_millis(delay_ms), task);
  promise
}

#[inline]
fn rt_async_sleep_legacy_impl(delay_ms: u64) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();

  extern "C" fn resolve_sleep(data: *mut u8) {
    let promise = PromiseRef(data.cast());
    async_rt::promise::promise_resolve(promise, core::ptr::null_mut());
  }

  let promise = async_rt::promise::promise_new();
  let _timer_id = async_rt::global().schedule_timer_in(
    Duration::from_millis(delay_ms),
    async_rt::Task::new(resolve_sleep, promise.0 as *mut u8),
  );
  promise
}

/// Resolve a promise after `delay_ms` milliseconds.
///
/// This is a small convenience helper for generated code and embeddings that want a
/// promise-based sleep primitive without implementing it in userland.
///
/// The returned promise is compatible with the native async/await ABI (`PromiseHeader` prefix).
#[no_mangle]
pub extern "C" fn rt_async_sleep(delay_ms: u64) -> PromiseRef {
  abort_on_panic(|| rt_async_sleep_impl(delay_ms))
}

#[no_mangle]
pub extern "C" fn rt_async_sleep_legacy(delay_ms: u64) -> PromiseRef {
  abort_on_panic(|| rt_async_sleep_legacy_impl(delay_ms))
}

// -----------------------------------------------------------------------------
// I/O readiness watchers (reactor-backed)
// -----------------------------------------------------------------------------

/// Debug-only error codes for the `rt_io_*` C ABI entrypoints.
///
/// These entrypoints cannot return a rich `io::Error` over the stable C ABI (e.g.
/// `rt_io_register` returns 0 on failure). The codes below allow tests (and
/// embedders in debug builds) to diagnose common failure modes.
///
/// The numeric values are **not** part of the stable ABI contract; this module is
/// `#[doc(hidden)]` and intended for tests/debugging only.
#[doc(hidden)]
pub mod rt_io_debug {
  /// No error was recorded (success or no `rt_io_*` call yet on this thread).
  pub const OK: u32 = 0;
  /// `interests` did not include `RT_IO_READABLE` and/or `RT_IO_WRITABLE`.
  pub const ERR_INVALID_INTERESTS: u32 = 1;
  /// The fd was not `O_NONBLOCK` (required by edge-triggered reactor contract).
  pub const ERR_FD_NOT_NONBLOCKING: u32 = 2;
  /// The fd is already registered with the reactor.
  pub const ERR_ALREADY_REGISTERED: u32 = 3;
  /// Some other error occurred while registering the fd.
  pub const ERR_OTHER: u32 = 4;
  /// `rt_io_update` failed (invalid watcher id or fd no longer satisfies contract).
  pub const ERR_UPDATE_FAILED: u32 = 5;
  /// `rt_io_unregister` failed (invalid watcher id).
  pub const ERR_UNREGISTER_FAILED: u32 = 6;
}

thread_local! {
  static RT_IO_LAST_ERROR: Cell<u32> = Cell::new(rt_io_debug::OK);
}

#[inline]
fn rt_io_set_last_error(code: u32) {
  // This debug-only TLS key can be accessed from other TLS destructors during thread teardown. If
  // the key has already been destroyed, `LocalKey::with` would panic with `AccessError` and abort
  // the process (`abort_on_dtor_unwind`). Treat it as best-effort and ignore `AccessError`.
  let _ = RT_IO_LAST_ERROR.try_with(|c| c.set(code));
}

/// Test/debug helper: return and clear the last `rt_io_*` failure code for the
/// **current thread**.
///
/// This is `#[doc(hidden)]` because it is not part of the stable runtime-native
/// C ABI.
#[doc(hidden)]
pub extern "C" fn rt_io_debug_take_last_error() -> u32 {
  abort_on_panic(|| {
    RT_IO_LAST_ERROR
      .try_with(|c| {
        let code = c.get();
        c.set(rt_io_debug::OK);
        code
      })
      .unwrap_or(rt_io_debug::OK)
  })
}

fn maybe_log_rt_io_failure(op: &str, msg: impl core::fmt::Display) {
  // These functions are part of the stable C ABI surface, but they cannot return
  // a rich error (e.g. `rt_io_register` returns 0 on failure). Emit a best-effort
  // diagnostic to stderr in debug builds so failures (like registering a blocking
  // fd) are diagnosable.
  if cfg!(debug_assertions) {
    eprintln!("runtime-native: {op} failed: {msg}");
  }
}

#[repr(C)]
struct RootedIoWatcherData {
  cb: extern "C" fn(u32, *mut u8),
  root: async_rt::gc::Root,
}

extern "C" fn rooted_io_watcher_cb(events: u32, data: *mut u8) {
  let ctx = unsafe { &*(data as *const RootedIoWatcherData) };
  crate::ffi::invoke_cb2_u32(ctx.cb, events, ctx.root.ptr());
}

extern "C" fn drop_rooted_io_watcher_data(data: *mut u8) {
  unsafe {
    drop(Box::from_raw(data as *mut RootedIoWatcherData));
  }
}

#[repr(C)]
struct HandleIoWatcherData {
  cb: extern "C" fn(u32, *mut u8),
  handle: u64,
  drop_data: Option<extern "C" fn(*mut u8)>,
}

extern "C" fn handle_io_watcher_cb(events: u32, data: *mut u8) {
  let ctx = unsafe { &*(data as *const HandleIoWatcherData) };
  let ptr = rt_handle_load(ctx.handle);
  if ptr.is_null() {
    return;
  }
  crate::ffi::invoke_cb2_u32(ctx.cb, events, ptr);
}

extern "C" fn drop_handle_io_watcher_data(data: *mut u8) {
  // Safety: allocated by `Box::into_raw` in `rt_io_register_handle*`.
  let ctx = unsafe { Box::from_raw(data as *mut HandleIoWatcherData) };
  let ptr = rt_handle_load(ctx.handle);
  if !ptr.is_null() {
    if let Some(drop_data) = ctx.drop_data {
      crate::ffi::invoke_cb1(drop_data, ptr);
    }
  }
  rt_handle_free(ctx.handle);
}

/// Register an fd with the runtime's readiness reactor.
///
/// ## Nonblocking / edge-triggered contract
///
/// The runtime-native reactor uses **edge-triggered** readiness notifications. The
/// provided `fd` **must already be set to `O_NONBLOCK`** before calling this
/// function.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
///
/// `interests` must include `RT_IO_READABLE` and/or `RT_IO_WRITABLE` (it must not
/// be zero). To stop watching, call [`rt_io_unregister`].
///
/// On failure, this function returns `0`. In debug builds, failures are logged to
/// stderr to aid diagnosis. Tests may also call the `#[doc(hidden)]`
/// [`rt_io_debug_take_last_error`] helper to retrieve a coarse failure code.
#[no_mangle]
pub extern "C" fn rt_io_register(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    match async_rt::global().register_io(fd, interests, cb, data) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        if is_nonblocking_contract_violation {
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

/// Register an fd with the runtime's readiness reactor, with an explicit drop hook for `data`.
///
/// `drop_data(data)` is invoked exactly once when the watcher is unregistered or cleared by runtime
/// teardown (`async_rt::clear_state_for_tests`). This ensures callback state can be freed even when
/// queued work is discarded.
///
/// ## Nonblocking / edge-triggered contract
///
/// Like [`rt_io_register`], the provided `fd` **must already be set to `O_NONBLOCK`**.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
///
/// On registration failure (return value `0`), `drop_data(data)` is still invoked and the runtime
/// does not retain the pointer.
#[no_mangle]
pub extern "C" fn rt_io_register_with_drop(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register_with_drop",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      ensure_current_thread_registered();
      crate::ffi::invoke_cb1(drop_data, data);
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    match async_rt::global().register_io_with_drop(fd, interests, cb, data, drop_data) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        crate::ffi::invoke_cb1(drop_data, data);
        if is_nonblocking_contract_violation {
          maybe_log_rt_io_failure(
            "rt_io_register_with_drop",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register_with_drop",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Like [`rt_io_register`], but keeps `data` alive as a GC root until the watcher is unregistered.
///
/// Contract:
/// - `data` must be the base pointer of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until `rt_io_unregister(id)` is called.
///
/// ## Nonblocking / edge-triggered contract
///
/// The provided `fd` **must already be set to `O_NONBLOCK`**. The runtime does not modify caller
/// file descriptor flags.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
#[no_mangle]
pub extern "C" fn rt_io_register_rooted(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register_rooted",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();

    let ctx = Box::new(RootedIoWatcherData {
      cb,
      // Safety: rooted entrypoints require `data` be a GC-managed object base pointer.
      root: unsafe { async_rt::gc::Root::new_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    match async_rt::global().register_io_with_drop(
      fd,
      interests,
      rooted_io_watcher_cb,
      ctx_ptr,
      drop_rooted_io_watcher_data,
    ) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        // Registration failed; drop the rooted wrapper to avoid leaking the persistent handle.
        drop_rooted_io_watcher_data(ctx_ptr);
        if is_nonblocking_contract_violation {
          maybe_log_rt_io_failure(
            "rt_io_register_rooted",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register_rooted",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Like [`rt_io_register_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
///
/// ## Nonblocking / edge-triggered contract
///
/// Like [`rt_io_register`], the provided `fd` **must already be set to `O_NONBLOCK`**. The runtime
/// does not modify caller file descriptor flags.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
#[no_mangle]
pub unsafe extern "C" fn rt_io_register_rooted_h(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: crate::roots::GcHandle,
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register_rooted_h",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();

    let ctx = Box::new(RootedIoWatcherData {
      cb,
      // Safety: caller contract.
      root: unsafe { async_rt::gc::Root::new_from_slot_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    match async_rt::global().register_io_with_drop(
      fd,
      interests,
      rooted_io_watcher_cb,
      ctx_ptr,
      drop_rooted_io_watcher_data,
    ) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        // Registration failed; drop the rooted wrapper to avoid leaking the persistent handle.
        drop_rooted_io_watcher_data(ctx_ptr);
        if is_nonblocking_contract_violation {
          maybe_log_rt_io_failure(
            "rt_io_register_rooted_h",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register_rooted_h",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Like [`rt_io_register`], but the callback userdata is a GC-rooted persistent handle.
///
/// Ownership:
/// - The runtime consumes `data` and treats it as a strong GC root while the watcher is registered.
/// - The runtime frees the handle exactly once when the watcher is unregistered (or if registration
///   fails).
///
/// If `data` is stale (freed), readiness callbacks are treated as no-ops.
///
/// ## Nonblocking / edge-triggered contract
///
/// Like [`rt_io_register`], the provided `fd` **must already be set to `O_NONBLOCK`**. The runtime
/// does not modify caller file descriptor flags.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
#[no_mangle]
pub extern "C" fn rt_io_register_handle(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: u64,
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register_handle",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      ensure_current_thread_registered();
      rt_handle_free(data);
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();

    let ctx = Box::new(HandleIoWatcherData {
      cb,
      handle: data,
      drop_data: None,
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    match async_rt::global().register_io_with_drop(
      fd,
      interests,
      handle_io_watcher_cb,
      ctx_ptr,
      drop_handle_io_watcher_data,
    ) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        drop_handle_io_watcher_data(ctx_ptr);
        if is_nonblocking_contract_violation {
          maybe_log_rt_io_failure(
            "rt_io_register_handle",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register_handle",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Like [`rt_io_register_handle`], but provides a teardown hook for the GC-rooted userdata.
///
/// `drop_data` is invoked exactly once when the watcher is unregistered or torn down (including on
/// registration failure), and runs before the runtime frees the handle.
///
/// ## Nonblocking / edge-triggered contract
///
/// Like [`rt_io_register`], the provided `fd` **must already be set to `O_NONBLOCK`**. The runtime
/// does not modify caller file descriptor flags.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration.
#[no_mangle]
pub extern "C" fn rt_io_register_handle_with_drop(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: u64,
  drop_data: extern "C" fn(*mut u8),
) -> IoWatcherId {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_register_handle_with_drop",
        format_args!(
          "fd={fd} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE)"
        ),
      );
      ensure_current_thread_registered();
      let ptr = rt_handle_load(data);
      if !ptr.is_null() {
        crate::ffi::invoke_cb1(drop_data, ptr);
      }
      rt_handle_free(data);
      return 0;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();

    let ctx = Box::new(HandleIoWatcherData {
      cb,
      handle: data,
      drop_data: Some(drop_data),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    match async_rt::global().register_io_with_drop(
      fd,
      interests,
      handle_io_watcher_cb,
      ctx_ptr,
      drop_handle_io_watcher_data,
    ) {
      Ok(id) => id.as_raw(),
      Err(err) => {
        let is_nonblocking_contract_violation =
          err.kind() == io::ErrorKind::InvalidInput && err.raw_os_error().is_none();
        let code = if is_nonblocking_contract_violation {
          rt_io_debug::ERR_FD_NOT_NONBLOCKING
        } else if err.kind() == io::ErrorKind::AlreadyExists {
          rt_io_debug::ERR_ALREADY_REGISTERED
        } else {
          rt_io_debug::ERR_OTHER
        };
        rt_io_set_last_error(code);

        drop_handle_io_watcher_data(ctx_ptr);
        if is_nonblocking_contract_violation {
          maybe_log_rt_io_failure(
            "rt_io_register_handle_with_drop",
            format_args!(
              "fd={fd} interests=0x{interests:x}: {err} (did you forget to set O_NONBLOCK?)"
            ),
          );
        } else {
          maybe_log_rt_io_failure(
            "rt_io_register_handle_with_drop",
            format_args!("fd={fd} interests=0x{interests:x}: {err}"),
          );
        }
        0
      }
    }
  })
}

/// Update the interest mask for an I/O watcher created by any of:
/// - [`rt_io_register`]
/// - [`rt_io_register_with_drop`]
/// - [`rt_io_register_rooted`]
/// - [`rt_io_register_rooted_h`]
/// - [`rt_io_register_handle`]
/// - [`rt_io_register_handle_with_drop`]
///
/// If the watcher is invalid or the underlying fd no longer satisfies the
/// nonblocking contract, the update is ignored. In debug builds, failures are
/// logged to stderr.
///
/// Tests may call the `#[doc(hidden)]` [`rt_io_debug_take_last_error`] helper to
/// retrieve a coarse failure code.
///
/// `interests` must include `RT_IO_READABLE` and/or `RT_IO_WRITABLE` (it must not
/// be zero). To stop watching, call [`rt_io_unregister`].
#[no_mangle]
pub extern "C" fn rt_io_update(id: IoWatcherId, interests: u32) {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    if interests & (crate::abi::RT_IO_READABLE | crate::abi::RT_IO_WRITABLE) == 0 {
      rt_io_set_last_error(rt_io_debug::ERR_INVALID_INTERESTS);
      maybe_log_rt_io_failure(
        "rt_io_update",
        format_args!(
          "id={id} interests=0x{interests:x}: invalid interest mask (must include RT_IO_READABLE and/or RT_IO_WRITABLE; use rt_io_unregister to stop watching)"
        ),
      );
      return;
    }
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    if !async_rt::global().update_io(WatcherId::from_raw(id), interests) {
      rt_io_set_last_error(rt_io_debug::ERR_UPDATE_FAILED);
      maybe_log_rt_io_failure(
        "rt_io_update",
        format_args!(
          "id={id} interests=0x{interests:x}: update failed (invalid id or fd no longer nonblocking)"
        ),
      );
    }
  })
}

/// Unregister an I/O watcher created by any of:
/// - [`rt_io_register`]
/// - [`rt_io_register_with_drop`]
/// - [`rt_io_register_rooted`]
/// - [`rt_io_register_rooted_h`]
/// - [`rt_io_register_handle`]
/// - [`rt_io_register_handle_with_drop`]
///
/// If the watcher is invalid, this is a no-op. In debug builds, failures are
/// logged to stderr.
///
/// Tests may call the `#[doc(hidden)]` [`rt_io_debug_take_last_error`] helper to
/// retrieve a coarse failure code.
#[no_mangle]
pub extern "C" fn rt_io_unregister(id: IoWatcherId) {
  abort_on_panic(|| {
    rt_io_set_last_error(rt_io_debug::OK);
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    if !async_rt::global().deregister_fd(WatcherId::from_raw(id)) {
      rt_io_set_last_error(rt_io_debug::ERR_UNREGISTER_FAILED);
      maybe_log_rt_io_failure(
        "rt_io_unregister",
        format_args!("id={id}: unregister failed (invalid id)"),
      );
    }
  })
}

// -----------------------------------------------------------------------------
// Timers (setTimeout/setInterval)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebTimerKind {
  Timeout,
  Interval,
}

#[derive(Clone)]
struct WebTimerState {
  kind: WebTimerKind,
  cb: async_rt::TaskFn,
  data: *mut u8,
  drop_data: Option<async_rt::TaskDropFn>,
  gc_root: Option<async_rt::gc::Root>,
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

// Safety: `WebTimerState` is stored behind a mutex in a process-global map. The runtime never
// dereferences `data`; it is passed back to user callbacks on the event-loop thread. Allowing it to
// cross thread boundaries is therefore safe as far as the runtime is concerned (FFI callers are
// responsible for ensuring their pointers remain valid). When `gc_root` is present, the GC-managed
// object is kept alive via the global persistent handle table and callbacks are invoked with the
// current relocated pointer.
unsafe impl Send for WebTimerState {}

static NEXT_WEB_TIMER_ID: AtomicU64 = AtomicU64::new(1);
static WEB_TIMERS: Lazy<GcAwareMutex<HashMap<TimerId, WebTimerState>>> =
  Lazy::new(|| GcAwareMutex::new(HashMap::new()));

pub(crate) fn clear_web_timers() {
  let mut timers = WEB_TIMERS.lock();
  for (_, st) in timers.drain() {
    if let Some(drop_data) = st.drop_data {
      crate::ffi::invoke_cb1(drop_data, st.data);
    }
  }
}

pub(crate) fn clear_web_timers_for_tests() {
  clear_web_timers();
}

/// Debug/test helper: hold the global web-timer registry lock (`WEB_TIMERS`).
///
/// This is intentionally *not* a stable public API. It exists so integration
/// tests can deterministically force contention on `WEB_TIMERS`.
#[doc(hidden)]
pub fn debug_hold_web_timers_lock() -> impl Drop {
  struct Hold {
    _guard: parking_lot::MutexGuard<'static, HashMap<TimerId, WebTimerState>>,
  }

  impl Drop for Hold {
    fn drop(&mut self) {}
  }

  Hold {
    _guard: WEB_TIMERS.lock(),
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

  let (kind, cb, data_ptr, gc_root, interval, drop_data) = {
    let mut timers = WEB_TIMERS.lock();
    let Some(snapshot) = timers.get(&id).cloned() else {
      return;
    };

    match snapshot.kind {
      WebTimerKind::Timeout => {
        let st = timers.remove(&id).expect("timer entry disappeared");
        (
          WebTimerKind::Timeout,
          st.cb,
          st.data,
          st.gc_root,
          Duration::ZERO,
          st.drop_data,
        )
      }
      WebTimerKind::Interval => {
        let st = timers.get_mut(&id).expect("timer entry disappeared");
        st.firing = true;
        (
          WebTimerKind::Interval,
          snapshot.cb,
          snapshot.data,
          snapshot.gc_root,
          snapshot.interval,
          snapshot.drop_data,
        )
      }
    }
  };

  let cb_data = gc_root.as_ref().map(|r| r.ptr()).unwrap_or(data_ptr);
  crate::ffi::invoke_cb1(cb, cb_data);

  if kind == WebTimerKind::Timeout {
    if let Some(drop_data) = drop_data {
      crate::ffi::invoke_cb1(drop_data, data_ptr);
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
    crate::ffi::invoke_cb1(drop_data, cb_data);
    return;
  }

  // HTML clamps nested timers to >= 4ms after a nesting depth of 5. The native runtime does not
  // currently track nesting; higher layers can implement clamping policy if needed.
  let now = async_rt::global().now();
  let deadline = now.checked_add(interval).unwrap_or(now);
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
pub extern "C" fn rt_queue_microtask_rooted(cb: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    unsafe {
      async_rt::global().enqueue_microtask(async_rt::Task::new_gc_rooted(cb, data));
    }
  })
}

/// Like [`rt_queue_microtask_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_queue_microtask_rooted_h(
  cb: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    unsafe {
      async_rt::global().enqueue_microtask(async_rt::Task::new_gc_rooted_h(cb, data));
    }
  })
}

#[no_mangle]
pub extern "C" fn rt_queue_microtask_with_drop(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  drop_data: extern "C" fn(*mut u8),
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    // Function pointers are non-null by construction in Rust, but this is a C ABI entrypoint and
    // may be called with null pointers from foreign code.
    if (cb as usize) == 0 || (drop_data as usize) == 0 {
      std::process::abort();
    }
    enqueue_microtask_with_optional_drop(cb, data, Some(drop_data));
  })
}

#[repr(C)]
struct HandleMicrotaskData {
  cb: extern "C" fn(*mut u8),
  handle: u64,
  drop_data: Option<extern "C" fn(*mut u8)>,
}

extern "C" fn handle_microtask_run(data: *mut u8) {
  // Safety: `data` is allocated by `Box::into_raw` in `rt_queue_microtask_handle*` and freed by the
  // task drop hook.
  let task = unsafe { &*(data as *const HandleMicrotaskData) };
  let ptr = rt_handle_load(task.handle);
  if ptr.is_null() {
    return;
  }
  crate::ffi::invoke_cb1(task.cb, ptr);
}

extern "C" fn handle_microtask_drop(data: *mut u8) {
  // Safety: allocated by `Box::into_raw` in `rt_queue_microtask_handle*`.
  let task = unsafe { Box::from_raw(data as *mut HandleMicrotaskData) };
  let ptr = rt_handle_load(task.handle);
  if !ptr.is_null() {
    if let Some(drop_data) = task.drop_data {
      crate::ffi::invoke_cb1(drop_data, ptr);
    }
  }
  rt_handle_free(task.handle);
}

/// Enqueue a microtask whose userdata is a GC-rooted persistent handle.
///
/// Ownership: the runtime consumes `data` and will free the handle when the microtask runs (or is
/// discarded).
#[no_mangle]
pub extern "C" fn rt_queue_microtask_handle(cb: extern "C" fn(*mut u8), data: u64) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    let task = Box::new(HandleMicrotaskData {
      cb,
      handle: data,
      drop_data: None,
    });
    async_rt::global().enqueue_microtask(async_rt::Task::new_with_drop(
      handle_microtask_run,
      Box::into_raw(task) as *mut u8,
      handle_microtask_drop,
    ));
  })
}

/// Like [`rt_queue_microtask_handle`], but provides a teardown hook for the userdata.
#[no_mangle]
pub extern "C" fn rt_queue_microtask_handle_with_drop(
  cb: extern "C" fn(*mut u8),
  data: u64,
  drop_data: extern "C" fn(*mut u8),
) {
  abort_on_panic(|| {
    let _ = crate::rt_ensure_init();
    ensure_current_thread_registered();
    let task = Box::new(HandleMicrotaskData {
      cb,
      handle: data,
      drop_data: Some(drop_data),
    });
    async_rt::global().enqueue_microtask(async_rt::Task::new_with_drop(
      handle_microtask_run,
      Box::into_raw(task) as *mut u8,
      handle_microtask_drop,
    ));
  })
}

#[repr(C)]
struct RootedWebTimerData {
  cb: extern "C" fn(*mut u8),
  root: async_rt::gc::Root,
}

extern "C" fn rooted_web_timer_cb(data: *mut u8) {
  let ctx = unsafe { &*(data as *const RootedWebTimerData) };
  crate::ffi::invoke_cb1(ctx.cb, ctx.root.ptr());
}

extern "C" fn drop_rooted_web_timer_data(data: *mut u8) {
  unsafe {
    drop(Box::from_raw(data as *mut RootedWebTimerData));
  }
}

#[repr(C)]
struct HandleWebTimerData {
  cb: extern "C" fn(*mut u8),
  handle: u64,
  drop_data: Option<extern "C" fn(*mut u8)>,
}

extern "C" fn handle_web_timer_cb(data: *mut u8) {
  let ctx = unsafe { &*(data as *const HandleWebTimerData) };
  let ptr = rt_handle_load(ctx.handle);
  if ptr.is_null() {
    return;
  }
  crate::ffi::invoke_cb1(ctx.cb, ptr);
}

extern "C" fn drop_handle_web_timer_data(data: *mut u8) {
  // Safety: allocated by `Box::into_raw` in `rt_set_*_handle*`.
  let ctx = unsafe { Box::from_raw(data as *mut HandleWebTimerData) };
  let ptr = rt_handle_load(ctx.handle);
  if !ptr.is_null() {
    if let Some(drop_data) = ctx.drop_data {
      crate::ffi::invoke_cb1(drop_data, ptr);
    }
  }
  rt_handle_free(ctx.handle);
}

#[no_mangle]
pub extern "C" fn rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb,
        data,
        drop_data: None,
        gc_root: None,
        interval: Duration::ZERO,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

/// Like [`rt_set_timeout`], but keeps `data` alive as a GC root until the timer fires or is cleared.
///
/// Contract:
/// - `data` must be the base pointer of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until the timeout fires or is cleared via
///   [`rt_clear_timer`].
#[no_mangle]
pub extern "C" fn rt_set_timeout_rooted(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  delay_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(RootedWebTimerData {
      cb,
      // Safety: rooted entrypoints require `data` be a GC-managed object base pointer.
      root: unsafe { async_rt::gc::Root::new_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb: rooted_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_rooted_web_timer_data),
        gc_root: None,
        interval: Duration::ZERO,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

/// Like [`rt_set_timeout_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_set_timeout_rooted_h(
  cb: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
  delay_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(RootedWebTimerData {
      cb,
      // Safety: caller contract.
      root: unsafe { async_rt::gc::Root::new_from_slot_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb: rooted_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_rooted_web_timer_data),
        gc_root: None,
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
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb,
        data,
        drop_data: Some(drop_data),
        gc_root: None,
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
pub extern "C" fn rt_set_timeout_handle(cb: extern "C" fn(*mut u8), data: u64, delay_ms: u64) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(HandleWebTimerData {
      cb,
      handle: data,
      drop_data: None,
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb: handle_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_handle_web_timer_data),
        gc_root: None,
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
pub extern "C" fn rt_set_timeout_handle_with_drop(
  cb: extern "C" fn(*mut u8),
  data: u64,
  drop_data: extern "C" fn(*mut u8),
  delay_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let delay = Duration::from_millis(delay_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(delay).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(HandleWebTimerData {
      cb,
      handle: data,
      drop_data: Some(drop_data),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Timeout,
        cb: handle_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_handle_web_timer_data),
        gc_root: None,
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
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb,
        data,
        drop_data: None,
        gc_root: None,
        interval,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

/// Like [`rt_set_interval`], but keeps `data` alive as a GC root until the interval is cleared.
///
/// Contract:
/// - `data` must be the base pointer of a GC-managed object (start of `ObjHeader`).
/// - The runtime registers a strong GC root for `data` until the interval is cleared via
///   [`rt_clear_timer`].
#[no_mangle]
pub extern "C" fn rt_set_interval_rooted(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  interval_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(RootedWebTimerData {
      cb,
      // Safety: rooted entrypoints require `data` be a GC-managed object base pointer.
      root: unsafe { async_rt::gc::Root::new_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb: rooted_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_rooted_web_timer_data),
        gc_root: None,
        interval,
        internal_id,
        firing: false,
        cancelled: false,
      },
    );
    id
  })
}

/// Like [`rt_set_interval_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
/// (pointer-to-slot).
///
/// # Safety
/// `data` must be a valid, aligned pointer to a writable `*mut u8` slot containing a GC-managed
/// object base pointer.
#[no_mangle]
pub unsafe extern "C" fn rt_set_interval_rooted_h(
  cb: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
  interval_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(RootedWebTimerData {
      cb,
      // Safety: caller contract.
      root: unsafe { async_rt::gc::Root::new_from_slot_unchecked(data) },
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb: rooted_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_rooted_web_timer_data),
        gc_root: None,
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
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb,
        data,
        drop_data: Some(drop_data),
        gc_root: None,
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
pub extern "C" fn rt_set_interval_handle(cb: extern "C" fn(*mut u8), data: u64, interval_ms: u64) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(HandleWebTimerData {
      cb,
      handle: data,
      drop_data: None,
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb: handle_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_handle_web_timer_data),
        gc_root: None,
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
pub extern "C" fn rt_set_interval_handle_with_drop(
  cb: extern "C" fn(*mut u8),
  data: u64,
  drop_data: extern "C" fn(*mut u8),
  interval_ms: u64,
) -> TimerId {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    let id = alloc_web_timer_id();
    let interval = Duration::from_millis(interval_ms);
    let now = async_rt::global().now();
    let deadline = now.checked_add(interval).unwrap_or(now);
    let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
    let internal_id = async_rt::global().schedule_timer(deadline, task);

    let ctx = Box::new(HandleWebTimerData {
      cb,
      handle: data,
      drop_data: Some(drop_data),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut u8;

    WEB_TIMERS.lock().insert(
      id,
      WebTimerState {
        kind: WebTimerKind::Interval,
        cb: handle_web_timer_cb,
        data: ctx_ptr,
        drop_data: Some(drop_handle_web_timer_data),
        gc_root: None,
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
      let Some(st) = timers.get(&id) else {
        return;
      };
      let kind = st.kind;
      let firing = st.firing;
      if kind == WebTimerKind::Interval && firing {
        let st = timers.get_mut(&id).expect("timer entry disappeared");
        st.cancelled = true;
        return;
      }
      let st = timers.remove(&id).expect("timer entry disappeared");
      let should_drop = st.drop_data.map(|f| (f, st.data));
      (st, should_drop)
    };
    let _ = async_rt::global().cancel_timer(st.internal_id);
    if let Some((drop_data, cb_data)) = should_drop {
      crate::ffi::invoke_cb1(drop_data, cb_data);
    }
  })
}

// -----------------------------------------------------------------------------
// Legacy promise/coroutine ABI (used by current async_rt tests)
// -----------------------------------------------------------------------------

// Compatibility layer: older tests/codegen used the unsuffixed `rt_promise_*` and `rt_coro_await`
// symbols. Keep them as forwarding shims to the `_legacy` implementations so external tooling can
// link against a stable name while the async ABI evolves.

#[no_mangle]
pub extern "C" fn rt_promise_new() -> PromiseRef {
  abort_on_panic(|| rt_promise_new_legacy())
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve(p: PromiseRef, value: ValueRef) {
  abort_on_panic(|| rt_promise_resolve_legacy(p, value))
}

#[no_mangle]
pub extern "C" fn rt_promise_then(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| rt_promise_then_legacy(p, on_settle, data))
}

#[no_mangle]
pub extern "C" fn rt_promise_then_rooted(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| rt_promise_then_rooted_legacy(p, on_settle, data))
}

#[no_mangle]
pub unsafe extern "C" fn rt_promise_then_rooted_h(
  p: PromiseRef,
  on_settle: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
) {
  abort_on_panic(|| unsafe { rt_promise_then_rooted_h_legacy(p, on_settle, data) })
}

#[no_mangle]
pub extern "C" fn rt_coro_await(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  abort_on_panic(|| rt_coro_await_legacy(coro, awaited, next_state))
}

#[no_mangle]
pub extern "C" fn rt_promise_new_legacy() -> PromiseRef {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_new()
  })
}

/// Return the payload buffer associated with a promise created by `rt_parallel_spawn_promise` (or
/// `rt_parallel_spawn_promise_rooted`), or the inline payload for a GC-managed promise created by
/// `rt_parallel_spawn_promise_with_shape`.
///
/// For non-payload promises, this may return null.
#[no_mangle]
pub extern "C" fn rt_promise_payload_ptr(p: PromiseRef) -> *mut u8 {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    if p.is_null() {
      return core::ptr::null_mut();
    }

    // `PromiseRef` is an opaque pointer in the stable ABI, but by contract it must point to a
    // `PromiseHeader` at offset 0 of the allocation.
    let header = p.0.cast::<PromiseHeader>();
    if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
      std::process::abort();
    }

    // Payload promises created by `rt_parallel_spawn_promise` are implemented by the legacy promise
    // runtime (`async_rt::promise`) and carry an out-of-line payload buffer.
    let flags = unsafe { &(*header).flags }.load(Ordering::Acquire);
    if (flags & crate::async_abi::PROMISE_FLAG_HAS_PAYLOAD) != 0 {
      return async_rt::promise::promise_payload_ptr(p);
    }

    // GC-managed native async ABI promises (`rt_alloc` + `rt_promise_init`) store their payload
    // inline immediately after the `PromiseHeader` prefix. We can detect them by the presence of a
    // non-null type descriptor in the GC header.
    //
    // Promises with an inert GC header (`type_desc == null`) include:
    // - legacy `async_rt::promise::RtPromise` promises (including old payload promises), and
    // - Rust `promise_api::Promise<T>` values allocated via `Arc`.
    let type_desc = unsafe { (*header).obj.type_desc };
    if type_desc.is_null() {
      return core::ptr::null_mut();
    }

    unsafe { (header as *mut u8).add(core::mem::size_of::<PromiseHeader>()) }
  })
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
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::promise::promise_resolve_into(p, value)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_promise_legacy(p: PromiseRef, other: PromiseRef) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::promise::promise_resolve_promise(p, other)
  })
}

/// Drop a legacy `async_rt::promise::RtPromise` allocated by `rt_promise_new_legacy`.
///
/// If `p` refers to a non-legacy promise allocation (e.g. a native async-ABI promise or a GC-managed
/// payload promise), this is a no-op.
#[no_mangle]
pub extern "C" fn rt_promise_drop_legacy(p: PromiseRef) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_drop(p);
  })
}

/// Test/debug helper: query a promise's current outcome without requiring access
/// to `async_rt::promise` internals.
#[doc(hidden)]
pub fn rt_debug_promise_outcome(p: PromiseRef) -> (u8, ValueRef) {
  ensure_event_loop_thread_registered();
  match async_rt::promise::promise_outcome(p) {
    async_rt::promise::PromiseOutcome::Pending => (0, core::ptr::null_mut()),
    async_rt::promise::PromiseOutcome::Fulfilled(v) => (1, v),
    async_rt::promise::PromiseOutcome::Rejected(e) => (2, e),
  }
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_thenable_legacy(p: PromiseRef, thenable: ThenableRef) {
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::promise::promise_resolve_thenable(p, thenable)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_then_legacy(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_then(p, on_settle, data)
  })
}

#[no_mangle]
pub extern "C" fn rt_promise_then_rooted_legacy(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    async_rt::promise::promise_then_rooted(p, on_settle, data)
  })
}

#[no_mangle]
pub unsafe extern "C" fn rt_promise_then_rooted_h_legacy(
  p: PromiseRef,
  on_settle: extern "C" fn(*mut u8),
  data: crate::roots::GcHandle,
) {
  abort_on_panic(|| {
    ensure_current_thread_registered();
    // Safety: caller contract.
    unsafe { async_rt::promise::promise_then_rooted_h(p, on_settle, data) }
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
  abort_on_panic(|| {
    ensure_event_loop_thread_registered();
    async_rt::coroutine::coro_await_value(coro, awaited, next_state)
  })
}

// -----------------------------------------------------------------------------
// Thread registration (native codegen / embedding)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_thread_current() -> *mut Thread {
  abort_on_panic(|| crate::thread::current_thread_ptr())
}

/// Attach the calling OS thread to `runtime`.
///
/// Returns a pointer to the per-thread [`Thread`] record, or null on failure.
///
/// # Safety
/// `runtime` must be a valid pointer to a [`Runtime`] created by the embedder.
#[no_mangle]
pub unsafe extern "C" fn rt_thread_attach(runtime: *mut Runtime) -> *mut Thread {
  abort_on_panic(|| unsafe {
    let Some(runtime) = runtime.as_ref() else {
      return std::ptr::null_mut();
    };

    match runtime.attach_current_thread_raw() {
      Ok(thread) => thread,
      Err(_) => std::ptr::null_mut(),
    }
  })
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
  // Capture the entrypoint FP before entering `abort_on_panic` for the same
  // reason as `rt_gc_safepoint_relocate_h`.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| unsafe {
    let Some(thread_ref) = thread.as_ref() else {
      return;
    };

    let runtime = thread_ref.runtime;
    let Some(runtime) = runtime.as_ref() else {
      return;
    };

    // Detach must not allow a thread to "disappear" from any runtime-visible
    // registries while a stop-the-world safepoint epoch is active.
    //
    // If the current thread participates in the safepoint registry, join the
    // safepoint first so the coordinator can make progress.
    if crate::threading::safepoint::current_epoch() & 1 == 1 {
      if registry::current_thread_id().is_some() {
        crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
          crate::threading::safepoint::rt_gc_safepoint();
        });
      } else {
        crate::threading::safepoint::wait_while_stop_the_world();
      }
    }

    // Best-effort: we cannot report errors over this C ABI.
    let _ = runtime.detach_thread_ptr(thread);
  })
}
