//! Runtime allocation fast paths.
//!
//! This module backs the `rt_alloc*` C ABI exports with:
//! - per-thread nursery TLAB bump allocation (`nursery::ThreadNursery`)
//! - per-thread Immix bump cursor within a reserved hole (`ImmixCursor`)
//!
//! The hot path performs no global locking.
//! Slow paths (Immix hole reservation, LOS allocation, GC) are serialized.

use crate::abi::RtShapeId;
use crate::array;
use crate::gc::heap::IMMIX_BLOCK_SIZE;
use crate::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use crate::gc::{ObjHeader, TypeDescriptor, YOUNG_SPACE, OBJ_ALIGN};
use crate::immix::LINE_SIZE;
use crate::nursery::{NurserySpace, ThreadNursery};
use crate::sync::GcAwareMutex;
use crate::threading::ThreadId;
use crate::shape_table;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;

/// Per-thread sampling period for checking nursery usage against
/// `HeapConfig::minor_gc_nursery_used_percent`.
///
/// The check itself reads a global atomic bump pointer and compares against a
/// static nursery size; keep it off the hot path by only checking occasionally.
const MINOR_GC_CHECK_PERIOD_BYTES: usize = 256 * 1024;

#[inline(always)]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
}

pub(crate) static NURSERY_EPOCH: AtomicU64 = AtomicU64::new(0);
pub(crate) static MAJOR_EPOCH: AtomicU64 = AtomicU64::new(0);

// Process-global heap config/limits.
//
// These can be configured via the exported C ABI (`rt_gc_set_config` /
// `rt_gc_set_limits`) but must be set before the global heap is initialized.
static GLOBAL_HEAP_CONFIG: Lazy<Mutex<crate::gc::config::HeapConfig>> =
  Lazy::new(|| Mutex::new(crate::gc::config::HeapConfig::default()));
static GLOBAL_HEAP_LIMITS: Lazy<Mutex<crate::gc::config::HeapLimits>> =
  Lazy::new(|| Mutex::new(crate::gc::config::HeapLimits::default()));
static GLOBAL_HEAP_CONFIG_SET: AtomicBool = AtomicBool::new(false);
static GLOBAL_HEAP_LIMITS_SET: AtomicBool = AtomicBool::new(false);

/// Global heap initialization state:
/// - 0: not started
/// - 1: initialization in progress
/// - 2: initialized
static GLOBAL_HEAP_INIT_STATE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn try_set_global_heap_config(config: crate::gc::config::HeapConfig) -> bool {
  let mut guard = GLOBAL_HEAP_CONFIG.lock();
  // Re-check under the config lock to avoid races where initialization starts between an unlocked
  // `GLOBAL_HEAP_INIT_STATE` check and updating the config value.
  if GLOBAL_HEAP_INIT_STATE.load(Ordering::Acquire) != 0 {
    return false;
  }
  *guard = config;
  GLOBAL_HEAP_CONFIG_SET.store(true, Ordering::Release);
  true
}

pub(crate) fn try_set_global_heap_limits(limits: crate::gc::config::HeapLimits) -> bool {
  let mut guard = GLOBAL_HEAP_LIMITS.lock();
  if GLOBAL_HEAP_INIT_STATE.load(Ordering::Acquire) != 0 {
    return false;
  }
  *guard = limits;
  GLOBAL_HEAP_LIMITS_SET.store(true, Ordering::Release);
  true
}

pub(crate) fn global_heap_config_snapshot() -> crate::gc::config::HeapConfig {
  if GLOBAL_HEAP_INIT_STATE.load(Ordering::Acquire) == 2 {
    let global = global_heap();
    // SAFETY: `global.heap` points to a leaked `GcHeap` that outlives the process.
    let heap = unsafe { &*(global.heap as *const crate::gc::GcHeap) };
    return *heap.config();
  }
  *GLOBAL_HEAP_CONFIG.lock()
}

pub(crate) fn global_heap_limits_snapshot() -> crate::gc::config::HeapLimits {
  if GLOBAL_HEAP_INIT_STATE.load(Ordering::Acquire) == 2 {
    let global = global_heap();
    // SAFETY: `global.heap` points to a leaked `GcHeap` that outlives the process.
    let heap = unsafe { &*(global.heap as *const crate::gc::GcHeap) };
    return *heap.limits();
  }
  *GLOBAL_HEAP_LIMITS.lock()
}

#[inline]
pub(crate) fn bump_nursery_epoch() {
  NURSERY_EPOCH.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn bump_major_epoch() {
  MAJOR_EPOCH.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug)]
struct ImmixCursor {
  cursor: *mut u8,
  limit: *mut u8,
}

impl ImmixCursor {
  pub const fn new() -> Self {
    Self {
      cursor: ptr::null_mut(),
      limit: ptr::null_mut(),
    }
  }

  #[inline(always)]
  pub fn alloc_fast(&mut self, size: usize, align: usize) -> Option<*mut u8> {
    debug_assert!(size != 0);
    debug_assert!(align.is_power_of_two());

    if self.cursor.is_null() {
      return None;
    }

    let cursor_addr = self.cursor as usize;
    let limit_addr = self.limit as usize;
    let aligned_addr = align_up(cursor_addr, align);
    let end_addr = aligned_addr.checked_add(size)?;
    if end_addr <= limit_addr {
      self.cursor = end_addr as *mut u8;
      Some(aligned_addr as *mut u8)
    } else {
      None
    }
  }

  #[inline]
  pub fn clear(&mut self) {
    self.cursor = ptr::null_mut();
    self.limit = ptr::null_mut();
  }
}

#[derive(Debug)]
struct ThreadAlloc {
  nursery: ThreadNursery,
  immix: ImmixCursor,
  nursery_epoch: u64,
  major_epoch: u64,
  minor_gc_check_debt: usize,
}

impl ThreadAlloc {
  pub const fn new() -> Self {
    Self {
      nursery: ThreadNursery::new(),
      immix: ImmixCursor::new(),
      nursery_epoch: 0,
      major_epoch: 0,
      minor_gc_check_debt: MINOR_GC_CHECK_PERIOD_BYTES,
    }
  }

  #[allow(dead_code)]
  #[inline]
  pub fn clear_after_minor(&mut self) {
    self.nursery.clear();
  }

  #[inline]
  pub fn clear_after_major(&mut self) {
    self.nursery.clear();
    self.immix.clear();
  }

  #[inline(always)]
  fn refresh_nursery_epoch(&mut self) {
    let global = NURSERY_EPOCH.load(Ordering::Relaxed);
    if self.nursery_epoch != global {
      self.nursery.clear();
      self.nursery_epoch = global;
    }
  }

  #[inline(always)]
  fn refresh_major_epoch(&mut self) {
    let global = MAJOR_EPOCH.load(Ordering::Relaxed);
    if self.major_epoch != global {
      self.immix.clear();
      self.major_epoch = global;
    }
  }
}

thread_local! {
  static TLS_ALLOC: UnsafeCell<ThreadAlloc> = UnsafeCell::new(ThreadAlloc::new());
  static TLS_ALLOC_REGISTERED: Cell<bool> = Cell::new(false);
}

#[inline]
fn ensure_thread_registered_for_alloc() {
  // Allocation must only happen from threads that participate in the safepoint protocol.
  //
  // Most callers will explicitly register threads via `rt_thread_init`, but Rust integration tests
  // (and some embedders) may call `rt_alloc` directly. `threading::register_current_thread` is
  // idempotent, so we can "ensure registered" on the first allocation on each thread.
  match TLS_ALLOC_REGISTERED.try_with(|flag| flag.get()) {
    Ok(true) => return,
    Ok(false) => {
      // This will call `on_thread_registered` (via the wrapper in `threading/mod.rs`), which sets
      // `TLS_ALLOC_REGISTERED` to true.
      crate::threading::register_current_thread(crate::threading::ThreadKind::External);
    }
    Err(_) => {
      // `rt_alloc` may be called from other thread-local destructors during thread teardown. If the
      // allocator TLS has already been destroyed, `LocalKey::with` would panic with `AccessError`
      // and abort the process (`abort_on_dtor_unwind`).
      //
      // Avoid calling into the allocator unless the thread is still registered with the runtime,
      // since allocating GC-managed objects from an unregistered thread would be unsound (GC would
      // not scan/stop this thread at safepoints).
      if crate::threading::registry::current_thread_state().is_some() {
        return;
      }

      crate::threading::register_current_thread(crate::threading::ThreadKind::External);
      if crate::threading::registry::current_thread_state().is_none() {
        crate::trap::rt_trap_invalid_arg("rt_alloc called during thread-local teardown");
      }
    }
  }
}

struct GlobalHeap {
  heap: usize,
  heap_lock: GcAwareMutex<()>,
}

fn global_heap() -> &'static GlobalHeap {
  static GLOBAL: OnceLock<GlobalHeap> = OnceLock::new();
  GLOBAL.get_or_init(|| {
    let mut config_guard = GLOBAL_HEAP_CONFIG.lock();
    let mut limits_guard = GLOBAL_HEAP_LIMITS.lock();

    // Freeze config/limits for this process-global heap instance. From this point onward, setter
    // calls must fail, and initialization must see a consistent snapshot.
    GLOBAL_HEAP_INIT_STATE.store(1, Ordering::Release);

    let mut config = *config_guard;
    let mut limits = *limits_guard;

    // Env overrides apply only to defaults: embedders that explicitly call `rt_gc_set_config` /
    // `rt_gc_set_limits` are expected to handle env overrides at a higher layer if desired.
    let apply_config = !GLOBAL_HEAP_CONFIG_SET.load(Ordering::Acquire);
    let apply_limits = !GLOBAL_HEAP_LIMITS_SET.load(Ordering::Acquire);

    crate::gc::config::apply_env_overrides(&mut config, &mut limits, apply_config, apply_limits);
    if let Err(msg) = config.validate() {
      crate::trap::rt_trap_invalid_arg(msg);
    }
    if let Err(msg) = limits.validate() {
      crate::trap::rt_trap_invalid_arg(msg);
    }
    if let Err(msg) = crate::gc::config::validate_config_and_limits(&config, &limits) {
      crate::trap::rt_trap_invalid_arg(msg);
    }

    // Publish the final config/limits (including any env overrides) so that `rt_gc_get_*` can return
    // the effective values even while heap initialization is in progress.
    *config_guard = config;
    *limits_guard = limits;
    drop(config_guard);
    drop(limits_guard);

    let mut heap = Box::new(crate::gc::GcHeap::with_config(config, limits));
    heap.reserve_card_table_objects_for_minor_gc();
    let heap_ptr = Box::into_raw(heap) as usize;

    // Initialize the write barrier's young-space range to the nursery backing this heap.
    //
    // Some tests install a synthetic young range (via `rt_gc_set_young_range`) to exercise the
    // exported write barrier without allocating a full GC heap. Avoid clobbering a non-empty range
    // that was explicitly configured by the test/embedding.
    if YOUNG_SPACE.start.load(Ordering::Acquire) == 0 && YOUNG_SPACE.end.load(Ordering::Acquire) == 0 {
      unsafe {
        let nursery = &(*(heap_ptr as *mut crate::gc::GcHeap)).nursery;
        YOUNG_SPACE
          .start
          .store(nursery.start() as usize, Ordering::Release);
        YOUNG_SPACE.end.store(nursery.end() as usize, Ordering::Release);
      }
    }

    let global = GlobalHeap {
      heap: heap_ptr,
      heap_lock: GcAwareMutex::new(()),
    };

    GLOBAL_HEAP_INIT_STATE.store(2, Ordering::Release);
    global
  })
}

#[inline]
fn nursery(global: &GlobalHeap) -> &NurserySpace {
  // SAFETY: `global.heap` points to a leaked `GcHeap` that outlives the process.
  unsafe { &(*(global.heap as *mut crate::gc::GcHeap)).nursery }
}

/// Ensure the process-global heap is initialized and that the write barrier's young range matches
/// its nursery.
///
/// Tests may reset the exported young range (`rt_gc_set_young_range`) between runs; allocator entry
/// points call this to restore the correct range.
pub(crate) fn ensure_global_heap_init() {
  let global = global_heap();
  unsafe {
    let nursery = &(*(global.heap as *mut crate::gc::GcHeap)).nursery;
    YOUNG_SPACE
      .start
      .store(nursery.start() as usize, Ordering::Release);
    YOUNG_SPACE.end.store(nursery.end() as usize, Ordering::Release);
  }
}

#[inline]
fn current_mark_epoch(global: &GlobalHeap) -> u8 {
  // SAFETY: `mark_epoch` is only mutated during stop-the-world GC. Mutator threads only read it
  // outside STW.
  unsafe { (*(global.heap as *mut crate::gc::GcHeap)).mark_epoch }
}

pub(crate) fn with_heap_lock_mutator<R>(f: impl FnOnce(&mut crate::gc::GcHeap) -> R) -> R {
  let global = global_heap();
  let _guard = global.heap_lock.lock();
  // SAFETY: `_guard` serializes access to the non-thread-safe parts of `GcHeap`.
  let heap = unsafe { &mut *(global.heap as *mut crate::gc::GcHeap) };
  f(heap)
}

/// Run `f` with exclusive access to the global heap while the world is stopped.
///
/// # Safety contract
/// Callers must ensure a stop-the-world (STW) safepoint is active: this must not run concurrently
/// with mutator threads.
pub(crate) fn with_heap_lock_world_stopped<R>(f: impl FnOnce(&mut crate::gc::GcHeap) -> R) -> R {
  let global = global_heap();
  // Root enumeration/relocation runs during stop-the-world (odd epoch). `GcAwareMutex::lock()` uses
  // a contended slow path that may temporarily enter a GC-safe region and may refuse to return while
  // a stop-the-world request is active for non-coordinator threads. Coordinator code must use
  // `lock_for_gc()` here so it can reliably acquire the heap lock while the world is stopped.
  let _guard = global.heap_lock.lock_for_gc();
  // SAFETY: `_guard` serializes access to the non-thread-safe parts of `GcHeap`.
  let heap = unsafe { &mut *(global.heap as *mut crate::gc::GcHeap) };
  f(heap)
}

/// Test-only hook: execute `f` while holding the process-global heap lock.
///
/// This exists so integration tests can deterministically force contention on the heap lock while
/// a thread requests stop-the-world GC. It is not considered stable API.
#[doc(hidden)]
pub(crate) fn debug_with_global_heap_lock<R>(f: impl FnOnce() -> R) -> R {
  let global = global_heap();
  let _guard = global.heap_lock.lock();
  f()
}

unsafe fn init_object(obj: *mut u8, size: usize, desc: &'static TypeDescriptor, epoch: u8, pinned: bool) {
  debug_assert!(!obj.is_null());

  #[cfg(any(debug_assertions, feature = "gc_debug"))]
  crate::gc::register_type_descriptor_ptr(desc as *const TypeDescriptor);

  // Ensure pointer slots start out as null so tracing never sees uninitialized garbage.
  ptr::write_bytes(obj, 0, size);

  let header = &mut *crate::gc::header_from_obj(obj);
  header.type_desc = desc as *const TypeDescriptor;
  header.meta.store(0, Ordering::Relaxed);
  header.set_mark_epoch(epoch);
  if pinned {
    header.set_pinned(true);
  }
}

pub(crate) fn on_thread_registered(_id: ThreadId) {
  // The thread-local allocator bookkeeping is best-effort. `threading::unregister_current_thread`
  // can be called from other TLS destructors during thread teardown; if this TLS key has already
  // been destroyed, `LocalKey::with` would panic with `AccessError` and abort the process
  // (`abort_on_dtor_unwind`).
  //
  // Treat `AccessError` as "already registered" and skip TLS updates.
  let should_init = TLS_ALLOC_REGISTERED
    .try_with(|flag| {
      if flag.get() {
        false
      } else {
        flag.set(true);
        true
      }
    })
    .unwrap_or(false);
  if !should_init {
    return;
  }

  let _ = TLS_ALLOC.try_with(|alloc| unsafe {
    let alloc = &mut *alloc.get();
    alloc.nursery_epoch = NURSERY_EPOCH.load(Ordering::Relaxed);
    alloc.major_epoch = MAJOR_EPOCH.load(Ordering::Relaxed);
  });

  // Keep write-barrier behavior deterministic for tests that intentionally set a synthetic
  // young-generation range (see `tests/write_barrier_integration.rs`). Only restore the global heap
  // nursery range if the range is currently unset *and* the global heap is already initialized.
  if YOUNG_SPACE.start.load(Ordering::Acquire) == 0 && YOUNG_SPACE.end.load(Ordering::Acquire) == 0 {
    if GLOBAL_HEAP_INIT_STATE.load(Ordering::Acquire) == 2 {
      ensure_global_heap_init();
    }
  }
}

pub(crate) fn on_thread_unregistered(_id: ThreadId) {
  // Like `on_thread_registered`, unregistration can run during TLS destruction (see
  // `tests/alloc_tls_teardown_unregister.rs`). Avoid panicking with `AccessError` in that case.
  let _ = TLS_ALLOC_REGISTERED.try_with(|flag| flag.set(false));
  let _ = TLS_ALLOC.try_with(|alloc| unsafe {
    (*alloc.get()).clear_after_major();
  });
}

#[inline(always)]
fn safepoint_poll_with_entry_fp(entry_fp: u64) {
  let epoch = crate::threading::safepoint::RT_GC_EPOCH.load(Ordering::Acquire);
  if epoch & 1 == 0 {
    return;
  }
  crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
    crate::threading::safepoint_poll();
  });
}

#[inline]
fn should_trigger_minor_gc_sampled(global: &GlobalHeap, size: usize) -> bool {
  // Sampling: decrement a per-thread "debt" counter so we only check nursery usage occasionally.
  let should_check = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      if alloc.minor_gc_check_debt <= size {
        alloc.minor_gc_check_debt = MINOR_GC_CHECK_PERIOD_BYTES;
        true
      } else {
        alloc.minor_gc_check_debt -= size;
        false
      }
    })
    .unwrap_or(false);
  if !should_check {
    return false;
  }

  // SAFETY: Heap config is immutable after heap initialization.
  let percent = unsafe { (&*(global.heap as *const crate::gc::GcHeap)).config().minor_gc_nursery_used_percent };
  if percent >= 100 {
    return false;
  }

  let used = nursery(global).allocated_bytes() as u128;
  let cap = nursery(global).size_bytes() as u128;
  used.saturating_mul(100) > cap.saturating_mul(percent as u128)
}

#[inline]
fn should_trigger_major_gc_locked(heap: &crate::gc::GcHeap) -> bool {
  let cfg = heap.config();
  let old_bytes = (heap.immix.block_count() * IMMIX_BLOCK_SIZE).saturating_add(heap.los.committed_bytes());
  old_bytes > cfg.major_gc_old_bytes_threshold
    || heap.immix.block_count() > cfg.major_gc_old_blocks_threshold
    || heap.external_bytes() > cfg.major_gc_external_bytes_threshold
}

#[inline]
fn is_above_hard_limits_locked(heap: &crate::gc::GcHeap) -> bool {
  let limits = heap.limits();
  heap.estimated_total_bytes() > limits.max_heap_bytes || heap.estimated_total_bytes_including_external() > limits.max_total_bytes
}

pub(crate) fn alloc(size: usize, shape: RtShapeId, entry_fp: u64) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  safepoint_poll_with_entry_fp(entry_fp);
  ensure_global_heap_init();

  let (shape_desc, type_desc) = shape_table::validate_alloc_request(size, shape);
  let size = shape_desc.size as usize;
  debug_assert_eq!(
    size,
    type_desc.size,
    "shape table TypeDescriptor::size must match RtShapeDescriptor.size"
  );

  let shape_align = shape_desc.align as usize;
  if shape_align == 0 || !shape_align.is_power_of_two() {
    crate::trap::rt_trap_invalid_arg("rt_alloc: shape descriptor align must be a non-zero power of two");
  }
  let align = shape_align.max(OBJ_ALIGN);
  debug_assert_eq!(
    align,
    type_desc.align,
    "shape table TypeDescriptor::align must match max(OBJ_ALIGN, RtShapeDescriptor.align)"
  );

  let global = global_heap();
  let mut epoch = current_mark_epoch(global);

  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    // Large object space (LOS) allocation path.
    //
    // LOS allocations mutate global heap state and can exceed heap limits; check triggers and
    // retry after a major GC when needed.
    let mut did_major = false;
    loop {
      // Fast path: do not take the heap lock unless we are actually allocating.
      let action = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
        with_heap_lock_mutator(|heap| {
          if !did_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
            return Err(());
          }

          // Enforce hard limits deterministically *before* calling into `mmap`.
          //
          // Mirror `GcHeap::try_alloc_old_raw` LOS accounting: round up to 4k pages.
          let committed_bytes = size.saturating_add(align.saturating_sub(1));
          let committed = (committed_bytes + 4095) & !4095;
          let projected_heap = heap.projected_total_bytes_with(0, 1, committed);
          let projected_total = heap.projected_total_bytes_including_external_with(0, 1, committed);
          if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
            return Err(());
          }

          let obj = heap.los.alloc(size, align);
          let epoch = heap.mark_epoch;
          unsafe { init_object(obj, size, type_desc, epoch, false) };
          Ok(obj)
        })
      });

      match action {
        Ok(obj) => return obj,
        Err(()) => {
          if did_major {
            crate::trap::rt_trap_oom(size, "rt_alloc: LOS allocation exceeded heap limits");
          }
          did_major = true;
          crate::exports::gc_collect_major_for_alloc("rt_alloc", entry_fp);
        }
      }
    }
  }

  // Fast path: per-thread nursery TLAB.
  if should_trigger_minor_gc_sampled(global, size) {
    crate::exports::gc_collect_minor_for_alloc("rt_alloc", entry_fp);
  }
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, type_desc, epoch, false) };
    return obj;
  }

  // Nursery exhausted: trigger a minor collection and retry once before falling back to old-gen.
  crate::exports::gc_collect_minor_for_alloc("rt_alloc", entry_fp);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, type_desc, epoch, false) };
    return obj;
  }

  // Still cannot allocate in the nursery: run a major collection (also resets the nursery) and retry.
  crate::exports::gc_collect_major_for_alloc("rt_alloc", entry_fp);
  epoch = current_mark_epoch(global);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, type_desc, epoch, false) };
    return obj;
  }

  // Fall back to old-gen allocation.
  alloc_old_may_gc(size, align, type_desc, entry_fp, "rt_alloc")
}

pub(crate) fn alloc_array(len: usize, elem_size: usize, entry_fp: u64) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  safepoint_poll_with_entry_fp(entry_fp);
  ensure_global_heap_init();

  let Some(spec) = array::decode_rt_array_elem_size(elem_size) else {
    crate::trap::rt_trap_invalid_arg("rt_alloc_array: invalid elem_size");
  };
  let payload_bytes = array::checked_payload_bytes(len, spec.elem_size)
    .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("rt_alloc_array: size overflow"));
  let size = array::checked_total_bytes(len, spec.elem_size)
    .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("rt_alloc_array: size overflow"));

  let should_install_card_table = (spec.elem_flags & array::RT_ARRAY_FLAG_PTR_ELEMS) != 0
    && payload_bytes >= crate::gc::CARD_TABLE_MIN_BYTES;

  let align = array::RT_ARRAY_TYPE_DESC.align.max(OBJ_ALIGN);
  let global = global_heap();
  let mut epoch = current_mark_epoch(global);

  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    // Large object space (LOS) array allocation path.
    let mut did_major = false;
    loop {
      let res = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
        with_heap_lock_mutator(|heap| {
          if !did_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
            return Err(());
          }

          let committed_bytes = size.saturating_add(align.saturating_sub(1));
          let committed = (committed_bytes + 4095) & !4095;
          let projected_heap = heap.projected_total_bytes_with(0, 1, committed);
          let projected_total = heap.projected_total_bytes_including_external_with(0, 1, committed);
          if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
            return Err(());
          }

          let obj = heap.los.alloc(size, align);
          let epoch = heap.mark_epoch;
          unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
          unsafe {
            let arr = &mut *(obj as *mut array::RtArrayHeader);
            arr.len = len;
            arr.elem_size = spec.elem_size as u32;
            arr.elem_flags = spec.elem_flags;
          }
          if should_install_card_table {
            unsafe {
              heap.install_card_table_for_obj(&mut *(obj as *mut ObjHeader), size);
            }
          }
          Ok(obj)
        })
      });

      match res {
        Ok(obj) => return obj,
        Err(()) => {
          if did_major {
            crate::trap::rt_trap_oom(size, "rt_alloc_array: LOS allocation exceeded heap limits");
          }
          did_major = true;
          crate::exports::gc_collect_major_for_alloc("rt_alloc_array", entry_fp);
        }
      }
    }
  }

  if should_trigger_minor_gc_sampled(global, size) {
    crate::exports::gc_collect_minor_for_alloc("rt_alloc_array", entry_fp);
  }
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
    unsafe {
      let arr = &mut *(obj as *mut array::RtArrayHeader);
      arr.len = len;
      arr.elem_size = spec.elem_size as u32;
      arr.elem_flags = spec.elem_flags;
    }
    return obj;
  }

  // Nursery exhausted for this array: trigger a minor collection and retry once before falling back.
  crate::exports::gc_collect_minor_for_alloc("rt_alloc_array", entry_fp);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
    unsafe {
      let arr = &mut *(obj as *mut array::RtArrayHeader);
      arr.len = len;
      arr.elem_size = spec.elem_size as u32;
      arr.elem_flags = spec.elem_flags;
    }
    return obj;
  }

  // Still cannot allocate in the nursery: run a major collection (also resets the nursery) and retry.
  crate::exports::gc_collect_major_for_alloc("rt_alloc_array", entry_fp);
  epoch = current_mark_epoch(global);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
    unsafe {
      let arr = &mut *(obj as *mut array::RtArrayHeader);
      arr.len = len;
      arr.elem_size = spec.elem_size as u32;
      arr.elem_flags = spec.elem_flags;
    }
    return obj;
  }

  if should_install_card_table {
    // Card-table installation requires registering the owning object with the heap; keep the
    // allocation + installation atomic w.r.t. GC by doing the full operation under the heap lock.
    let mut did_major_for_pressure = false;
    let mut did_major_for_failure = false;
    loop {
      let attempt = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
        with_heap_lock_mutator(|heap| {
          if !did_major_for_pressure && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
            return Err(());
          }

          let obj = match TLS_ALLOC.try_with(|alloc| unsafe {
            let alloc = &mut *alloc.get();
            alloc.refresh_major_epoch();
            alloc.immix.alloc_fast(size, align)
          }) {
            Ok(Some(obj)) => obj,
            Ok(None) => {
              let min_lines = size.div_ceil(LINE_SIZE);
              // Prefer reusing an existing hole; only grow the Immix space if needed and within hard limits.
              if let Some((start, limit)) = heap.immix.reserve_hole(min_lines, false) {
                let _ = TLS_ALLOC.try_with(|alloc| unsafe {
                  let alloc = &mut *alloc.get();
                  alloc.refresh_major_epoch();
                  alloc.immix.cursor = start;
                  alloc.immix.limit = limit;
                });
              } else {
                let projected_heap = heap.projected_total_bytes_with(1, 0, 0);
                let projected_total = heap.projected_total_bytes_including_external_with(1, 0, 0);
                if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
                  return Err(());
                }
                let Some((start, limit)) = heap.immix.reserve_hole(min_lines, true) else {
                  return Err(());
                };
                let _ = TLS_ALLOC.try_with(|alloc| unsafe {
                  let alloc = &mut *alloc.get();
                  alloc.refresh_major_epoch();
                  alloc.immix.cursor = start;
                  alloc.immix.limit = limit;
                });
              }

              match TLS_ALLOC.try_with(|alloc| unsafe {
                let alloc = &mut *alloc.get();
                alloc.refresh_major_epoch();
                alloc.immix.alloc_fast(size, align)
              }) {
                Ok(Some(obj)) => obj,
                Ok(None) => return Err(()),
                Err(_) => heap.los.alloc(size, align),
              }
            }
            Err(_) => heap.los.alloc(size, align),
          };

          let epoch = heap.mark_epoch;
          unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
          unsafe {
            let arr = &mut *(obj as *mut array::RtArrayHeader);
            arr.len = len;
            arr.elem_size = spec.elem_size as u32;
            arr.elem_flags = spec.elem_flags;
          }

          unsafe {
            heap.install_card_table_for_obj(&mut *(obj as *mut ObjHeader), size);
          }
          Ok(obj)
        })
      });

      match attempt {
        Ok(obj) => return obj,
        Err(()) => {
          if !did_major_for_pressure {
            did_major_for_pressure = true;
            crate::exports::gc_collect_major_for_alloc("rt_alloc_array", entry_fp);
            continue;
          }
          if did_major_for_failure {
            crate::trap::rt_trap_oom(size, "rt_alloc_array: Immix out of space");
          }
          did_major_for_failure = true;
          crate::exports::gc_collect_major_for_alloc("rt_alloc_array", entry_fp);
        }
      }
    }
  }

  // Fall back to old-gen allocation.
  let obj = alloc_old_may_gc(size, align, &array::RT_ARRAY_TYPE_DESC, entry_fp, "rt_alloc_array");
  unsafe {
    let arr = &mut *(obj as *mut array::RtArrayHeader);
    arr.len = len;
    arr.elem_size = spec.elem_size as u32;
    arr.elem_flags = spec.elem_flags;
  }
  obj
}

pub(crate) fn alloc_pinned(size: usize, shape: RtShapeId, entry_fp: u64) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  safepoint_poll_with_entry_fp(entry_fp);
  ensure_global_heap_init();

  let (shape_desc, type_desc) = shape_table::validate_alloc_request(size, shape);
  let size = shape_desc.size as usize;
  debug_assert_eq!(
    size,
    type_desc.size,
    "shape table TypeDescriptor::size must match RtShapeDescriptor.size"
  );

  let shape_align = shape_desc.align as usize;
  if shape_align == 0 || !shape_align.is_power_of_two() {
    crate::trap::rt_trap_invalid_arg(
      "rt_alloc_pinned: shape descriptor align must be a non-zero power of two",
    );
  }
  let align = shape_align.max(OBJ_ALIGN);
  debug_assert_eq!(
    align,
    type_desc.align,
    "shape table TypeDescriptor::align must match max(OBJ_ALIGN, RtShapeDescriptor.align)"
  );

  let mut did_major = false;
  loop {
    let res = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
      with_heap_lock_mutator(|heap| {
        if !did_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
          return Err(());
        }

        // Mirror `GcHeap::try_alloc_pinned` LOS accounting (round to 4k).
        let committed_bytes = size.saturating_add(align.saturating_sub(1));
        let committed = (committed_bytes + 4095) & !4095;
        let projected_heap = heap.projected_total_bytes_with(0, 1, committed);
        let projected_total = heap.projected_total_bytes_including_external_with(0, 1, committed);
        if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
          return Err(());
        }

        let obj = heap.los.alloc(size, align);
        let epoch = heap.mark_epoch;
        unsafe { init_object(obj, size, type_desc, epoch, true) };
        Ok(obj)
      })
    });

    match res {
      Ok(obj) => return obj,
      Err(()) => {
        if did_major {
          crate::trap::rt_trap_oom(size, "rt_alloc_pinned: exceeded heap limits");
        }
        did_major = true;
        crate::exports::gc_collect_major_for_alloc("rt_alloc_pinned", entry_fp);
      }
    }
  }
}

/// Allocate a GC object in the process-global heap using a custom [`TypeDescriptor`].
///
/// This is intended for runtime-native subsystems (like the string interner) that need dynamic
/// object sizes that cannot be represented in the static shape table used by `rt_alloc`.
///
/// The object is always allocated in the old generation (Immix or LOS), matching the interner's
/// expectation that interned byte storage has a stable address across minor collections.
///
/// Prefer using [`alloc_typed_old`] for new runtime-owned types; this function is kept for
/// compatibility with existing runtime-native subsystems.
#[allow(dead_code)]
pub(crate) fn alloc_old_with_type_desc(desc: &'static TypeDescriptor) -> *mut u8 {
  // This function is retained for backwards compatibility with existing runtime-native subsystems.
  // Prefer `alloc_typed_old`, which requires the caller to supply an `entry_fp` for stackmap fixups
  // when allocator-triggered GC runs inside `abort_on_panic` / `catch_unwind` frames.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  alloc_typed_old_with_entry(desc, entry_fp, "alloc_old_with_type_desc")
}

/// Allocate a GC-managed object using a runtime-owned [`TypeDescriptor`].
///
/// This mirrors the behavior of [`alloc`] (nursery preferred → old-gen → LOS),
/// but bypasses the shape table. It exists for runtime-owned allocation kinds
/// like `RtString` that are not part of the compiler shape table.
#[allow(dead_code)]
pub(crate) fn alloc_typed(desc: &'static TypeDescriptor) -> *mut u8 {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  alloc_typed_with_entry(desc, entry_fp, "alloc_typed")
}

/// Like [`alloc_typed`], but allows the caller to supply the outer runtime entrypoint's frame
/// pointer.
///
/// Providing an `entry_fp` captured **before** entering `abort_on_panic` keeps stackmap fixups
/// reliable even when `catch_unwind` frames do not maintain a consistent frame-pointer chain.
pub(crate) fn alloc_typed_with_entry(
  desc: &'static TypeDescriptor,
  entry_fp: u64,
  entry_name: &'static str,
) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  safepoint_poll_with_entry_fp(entry_fp);
  ensure_global_heap_init();

  let size = desc.size;
  if size == 0 {
    crate::trap::rt_trap_invalid_arg("alloc_typed: TypeDescriptor.size must be non-zero");
  }

  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc(size);

  let align = desc.align.max(OBJ_ALIGN);
  let global = global_heap();
  let mut epoch = current_mark_epoch(global);

  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    // Large object space (LOS) allocation path.
    let mut did_major = false;
    loop {
      let action = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
        with_heap_lock_mutator(|heap| {
          if !did_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
            return Err(());
          }

          let committed_bytes = size.saturating_add(align.saturating_sub(1));
          let committed = (committed_bytes + 4095) & !4095;
          let projected_heap = heap.projected_total_bytes_with(0, 1, committed);
          let projected_total = heap.projected_total_bytes_including_external_with(0, 1, committed);
          if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
            return Err(());
          }

          let obj = heap.los.alloc(size, align);
          let epoch = heap.mark_epoch;
          unsafe { init_object(obj, size, desc, epoch, false) };
          Ok(obj)
        })
      });

      match action {
        Ok(obj) => return obj,
        Err(()) => {
          if did_major {
            crate::trap::rt_trap_oom(size, "alloc_typed: LOS allocation exceeded heap limits");
          }
          did_major = true;
          crate::exports::gc_collect_major_for_alloc(entry_name, entry_fp);
        }
      }
    }
  }

  if should_trigger_minor_gc_sampled(global, size) {
    crate::exports::gc_collect_minor_for_alloc(entry_name, entry_fp);
  }
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }

  // Nursery exhausted: trigger a minor collection and retry once.
  crate::exports::gc_collect_minor_for_alloc(entry_name, entry_fp);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }

  // Still cannot allocate in the nursery: run a major collection (also resets the nursery) and retry.
  crate::exports::gc_collect_major_for_alloc(entry_name, entry_fp);
  epoch = current_mark_epoch(global);
  if let Some(obj) = TLS_ALLOC
    .try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_nursery_epoch();
      alloc.nursery.alloc(size, align, nursery(global))
    })
    .ok()
    .flatten()
  {
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }

  // Fall back to old-gen allocation.
  alloc_old_may_gc(size, align, desc, entry_fp, entry_name)
}

/// Allocate a GC-managed object directly into the old generation (Immix/LOS).
///
/// This is intended for runtime-owned allocations that should avoid nursery
/// evacuation overhead (e.g. weakly-interned strings).
#[allow(dead_code)]
pub(crate) fn alloc_typed_old(desc: &'static TypeDescriptor) -> *mut u8 {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  alloc_typed_old_with_entry(desc, entry_fp, "alloc_typed_old")
}

/// Like [`alloc_typed_old`], but allows supplying the outer runtime entrypoint's frame pointer.
pub(crate) fn alloc_typed_old_with_entry(
  desc: &'static TypeDescriptor,
  entry_fp: u64,
  entry_name: &'static str,
) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  safepoint_poll_with_entry_fp(entry_fp);
  ensure_global_heap_init();

  let size = desc.size;
  if size == 0 {
    crate::trap::rt_trap_invalid_arg("alloc_typed_old: TypeDescriptor.size must be non-zero");
  }

  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc(size);

  let align = desc.align.max(OBJ_ALIGN);
  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    // Large object space (LOS) allocation path.
    let mut did_major = false;
    loop {
      let action = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
        with_heap_lock_mutator(|heap| {
          if !did_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
            return Err(());
          }

          let committed_bytes = size.saturating_add(align.saturating_sub(1));
          let committed = (committed_bytes + 4095) & !4095;
          let projected_heap = heap.projected_total_bytes_with(0, 1, committed);
          let projected_total = heap.projected_total_bytes_including_external_with(0, 1, committed);
          if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
            return Err(());
          }

          let obj = heap.los.alloc(size, align);
          let epoch = heap.mark_epoch;
          unsafe { init_object(obj, size, desc, epoch, false) };
          Ok(obj)
        })
      });

      match action {
        Ok(obj) => return obj,
        Err(()) => {
          if did_major {
            crate::trap::rt_trap_oom(size, "alloc_typed_old: LOS allocation exceeded heap limits");
          }
          did_major = true;
          crate::exports::gc_collect_major_for_alloc(entry_name, entry_fp);
        }
      }
    }
  }

  alloc_old_may_gc(size, align, desc, entry_fp, entry_name)
}

fn alloc_old_may_gc(
  size: usize,
  align: usize,
  desc: &'static TypeDescriptor,
  entry_fp: u64,
  entry_name: &'static str,
) -> *mut u8 {
  let global = global_heap();

  // If allocator TLS is inaccessible during thread teardown, fall back to allocating in the LOS
  // without attempting to trigger GC (we may not have a stable managed callsite to publish).
  if TLS_ALLOC.try_with(|_| ()).is_err() {
    return crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
      with_heap_lock_mutator(|heap| {
        let epoch = current_mark_epoch(global);
        let obj = heap.los.alloc(size, align);
        unsafe { init_object(obj, size, desc, epoch, false) };
        obj
      })
    });
  }

  #[derive(Clone, Copy)]
  enum RefillResult {
    Hole(*mut u8, *mut u8),
    NeedMajor,
    Oom,
  }

  // Conservative estimate for how many Immix lines we need to reserve to satisfy
  // `alloc_fast` under arbitrary alignment padding.
  let committed_bytes = size.saturating_add(align.saturating_sub(1));
  let min_lines = committed_bytes.div_ceil(LINE_SIZE);

  let mut did_collect_major = false;
  loop {
    // Fast path: bump within the current thread-local Immix hole.
    match TLS_ALLOC.try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_major_epoch();
      alloc.immix.alloc_fast(size, align)
    }) {
      Ok(Some(obj)) => {
        let epoch = current_mark_epoch(global);
        unsafe { init_object(obj, size, desc, epoch, false) };
        return obj;
      }
      Ok(None) => {}
      Err(_) => {
        // We already validated TLS access above.
        unreachable!();
      }
    }

    // Slow path: reserve a new hole from the global Immix space.
    let refill = crate::threading::safepoint::with_safepoint_fixup_start_fp(entry_fp, || {
      with_heap_lock_mutator(|heap| {
        if !did_collect_major && (should_trigger_major_gc_locked(heap) || is_above_hard_limits_locked(heap)) {
          return RefillResult::NeedMajor;
        }

        // First, try to reuse an existing hole without growing the Immix space.
        if let Some((start, limit)) = heap.immix.reserve_hole(min_lines, false) {
          return RefillResult::Hole(start, limit);
        }

        // No holes available. Run a major GC before committing more memory.
        if !did_collect_major {
          return RefillResult::NeedMajor;
        }

        // Already did a major GC and still can't find space: attempt to grow by allocating a new
        // Immix block, but only if this stays within hard limits.
        let projected_heap = heap.projected_total_bytes_with(1, 0, 0);
        let projected_total = heap.projected_total_bytes_including_external_with(1, 0, 0);
        if projected_heap > heap.limits().max_heap_bytes || projected_total > heap.limits().max_total_bytes {
          return RefillResult::Oom;
        }

        let Some((start, limit)) = heap.immix.reserve_hole(min_lines, true) else {
          return RefillResult::Oom;
        };
        RefillResult::Hole(start, limit)
      })
    });

    let (start, limit) = match refill {
      RefillResult::Hole(start, limit) => (start, limit),
      RefillResult::NeedMajor => {
        if did_collect_major {
          crate::trap::rt_trap_oom(size, "rt_alloc: exceeded heap limits");
        }
        crate::exports::gc_collect_major_for_alloc(entry_name, entry_fp);
        did_collect_major = true;
        continue;
      }
      RefillResult::Oom => {
        crate::trap::rt_trap_oom(size, "rt_alloc: Immix out of space");
      }
    };

    let obj = match TLS_ALLOC.try_with(|alloc| unsafe {
      let alloc = &mut *alloc.get();
      alloc.refresh_major_epoch();
      alloc.immix.cursor = start;
      alloc.immix.limit = limit;
      alloc.immix.alloc_fast(size, align)
    }) {
      Ok(Some(obj)) => obj,
      Ok(None) => crate::trap::rt_trap_oom(size, "rt_alloc: Immix hole too small"),
      Err(_) => unreachable!(),
    };
    let epoch = current_mark_epoch(global);
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }
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
  fn heap_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

     // Stop-the-world handshakes can take much longer in debug builds (especially
     // under parallel test execution on multi-agent hosts). Keep release builds
     // strict, but give debug builds enough slack to avoid flaky timeouts.
     const TIMEOUT: Duration = if cfg!(debug_assertions) {
       Duration::from_secs(30)
     } else {
       Duration::from_secs(2)
     };

    // Ensure the heap is initialized so the lock exists.
    let heap = global_heap();

    std::thread::scope(|scope| {
      // Thread A holds the heap lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to allocate a large array (forces LOS allocation via heap lock).
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<usize>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = heap.heap_lock.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the heap lock");

       scope.spawn(move || {
         let id = threading::register_current_thread(ThreadKind::Worker);
         c_registered_tx.send(id).unwrap();
         c_start_rx.recv().unwrap();

        // Force a LOS allocation by exceeding `IMMIX_MAX_OBJECT_SIZE`.
        let entry_fp = crate::stackwalk::current_frame_pointer();
        let obj = alloc_array(IMMIX_MAX_OBJECT_SIZE + 1024, 1, entry_fp);
        c_done_tx.send(obj as usize).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the heap lock before starting STW.
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
          panic!("thread C did not enter a GC-safe region while blocked on the heap lock");
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
        "world failed to stop within timeout; heap lock contention must not block STW"
      );

      // Resume the world so the contending allocation can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      let obj = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("allocation should complete after world is resumed");
      assert_ne!(obj, 0);
    });
  }
}
