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
use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[inline(always)]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
}

pub(crate) static NURSERY_EPOCH: AtomicU64 = AtomicU64::new(0);
pub(crate) static MAJOR_EPOCH: AtomicU64 = AtomicU64::new(0);

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
}

impl ThreadAlloc {
  pub const fn new() -> Self {
    Self {
      nursery: ThreadNursery::new(),
      immix: ImmixCursor::new(),
      nursery_epoch: 0,
      major_epoch: 0,
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
  TLS_ALLOC_REGISTERED.with(|flag| {
    if flag.get() {
      return;
    }
    // This will call `on_thread_registered` (via the wrapper in `threading/mod.rs`), which sets
    // `TLS_ALLOC_REGISTERED` to true.
    crate::threading::register_current_thread(crate::threading::ThreadKind::External);
  });
}

struct GlobalHeap {
  heap: usize,
  heap_lock: GcAwareMutex<()>,
}

fn global_heap() -> &'static GlobalHeap {
  static GLOBAL: OnceLock<GlobalHeap> = OnceLock::new();
  GLOBAL.get_or_init(|| {
    let heap = Box::new(crate::gc::GcHeap::new());
    let heap_ptr = Box::into_raw(heap) as usize;

    // Initialize the write barrier's young-space range to the nursery backing this heap.
    unsafe {
      let nursery = &(*(heap_ptr as *mut crate::gc::GcHeap)).nursery;
      YOUNG_SPACE
        .start
        .store(nursery.start() as usize, Ordering::Release);
      YOUNG_SPACE.end.store(nursery.end() as usize, Ordering::Release);
    }

    GlobalHeap {
      heap: heap_ptr,
      heap_lock: GcAwareMutex::new(()),
    }
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

fn with_heap_lock_mutator<R>(f: impl FnOnce(&mut crate::gc::GcHeap) -> R) -> R {
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

  let header = &mut *(obj as *mut ObjHeader);
  header.type_desc = desc as *const TypeDescriptor;
  header.meta.store(0, Ordering::Relaxed);
  header.set_mark_epoch(epoch);
  if pinned {
    header.set_pinned(true);
  }
}

pub(crate) fn on_thread_registered(_id: ThreadId) {
  TLS_ALLOC_REGISTERED.with(|flag| {
    if flag.get() {
      return;
    }
    flag.set(true);
  });

  TLS_ALLOC.with(|alloc| unsafe {
    let alloc = &mut *alloc.get();
    alloc.nursery_epoch = NURSERY_EPOCH.load(Ordering::Relaxed);
    alloc.major_epoch = MAJOR_EPOCH.load(Ordering::Relaxed);
  });
}

pub(crate) fn on_thread_unregistered(_id: ThreadId) {
  TLS_ALLOC_REGISTERED.with(|flag| flag.set(false));
  TLS_ALLOC.with(|alloc| unsafe {
    (*alloc.get()).clear_after_major();
  });
}

pub(crate) fn alloc(size: usize, shape: RtShapeId) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  crate::threading::safepoint_poll();
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
  let epoch = current_mark_epoch(global);

  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    return with_heap_lock_mutator(|heap| {
      let obj = heap.los.alloc(size, align);
      unsafe { init_object(obj, size, type_desc, epoch, false) };
      obj
    });
  }

  // Fast path: per-thread nursery TLAB.
  if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe {
    let alloc = &mut *alloc.get();
    alloc.refresh_nursery_epoch();
    alloc.nursery.alloc(size, align, nursery(global))
  }) {
    unsafe { init_object(obj, size, type_desc, epoch, false) };
    return obj;
  }

  // Nursery exhausted: fall back to old-gen allocation.
  alloc_old(size, align, type_desc, epoch)
}

pub(crate) fn alloc_array(len: usize, elem_size: usize) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  crate::threading::safepoint_poll();
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
  let epoch = current_mark_epoch(global);

  if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
    return with_heap_lock_mutator(|heap| {
      let obj = heap.los.alloc(size, align);
      unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
      unsafe {
        let arr = &mut *(obj as *mut array::RtArrayHeader);
        arr.len = len;
        arr.elem_size = spec.elem_size as u32;
        arr.elem_flags = spec.elem_flags;
      }
      if should_install_card_table {
        // SAFETY: `obj` points at a heap allocation initialized by `init_object`.
        unsafe {
          heap.install_card_table_for_obj(&mut *(obj as *mut ObjHeader), size);
        }
      }
      obj
    });
  }

  if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe {
    let alloc = &mut *alloc.get();
    alloc.refresh_nursery_epoch();
    alloc.nursery.alloc(size, align, nursery(global))
  }) {
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
    // Installing a card table requires registering the owning object with the
    // heap so it can be reclaimed during major GC. Do the entire old-gen
    // allocation while holding the heap lock to avoid safepoint polls after the
    // object is allocated but before it is registered.
    return with_heap_lock_mutator(|heap| {
      // Fast path: bump within the current thread-local Immix hole.
      let obj = if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe { (*alloc.get()).immix.alloc_fast(size, align) }) {
        obj
      } else {
        // Slow path: reserve a new hole from the global Immix space.
        let min_lines = size.div_ceil(LINE_SIZE);
        let (start, limit) = heap
          .immix
          .reserve_hole(min_lines)
          .unwrap_or_else(|| crate::trap::rt_trap_oom(size, "rt_alloc_array: Immix out of space"));
        let obj = TLS_ALLOC.with(|alloc| unsafe {
          (*alloc.get()).immix.cursor = start;
          (*alloc.get()).immix.limit = limit;
          (*alloc.get()).immix.alloc_fast(size, align)
        });
        obj.unwrap_or_else(|| crate::trap::rt_trap_oom(size, "rt_alloc_array: Immix hole too small"))
      };

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
      obj
    });
  }

  let obj = alloc_old(size, align, &array::RT_ARRAY_TYPE_DESC, epoch);
  unsafe {
    let arr = &mut *(obj as *mut array::RtArrayHeader);
    arr.len = len;
    arr.elem_size = spec.elem_size as u32;
    arr.elem_flags = spec.elem_flags;
  }
  obj
}

pub(crate) fn alloc_pinned(size: usize, shape: RtShapeId) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  crate::threading::safepoint_poll();
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

  let global = global_heap();
  let epoch = current_mark_epoch(global);

  with_heap_lock_mutator(|heap| {
    let obj = heap.los.alloc(size, align);
    unsafe { init_object(obj, size, type_desc, epoch, true) };
    obj
  })
}

fn alloc_old(size: usize, align: usize, desc: &'static TypeDescriptor, epoch: u8) -> *mut u8 {
  // Fast path: bump within the current thread-local Immix hole.
  if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe {
    let alloc = &mut *alloc.get();
    alloc.refresh_major_epoch();
    alloc.immix.alloc_fast(size, align)
  }) {
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }

  // Slow path: reserve a new hole from the global Immix space.
  let min_lines = size.div_ceil(LINE_SIZE);
  let (start, limit) = with_heap_lock_mutator(|heap| heap.immix.reserve_hole(min_lines))
    .unwrap_or_else(|| crate::trap::rt_trap_oom(size, "rt_alloc: Immix out of space"));

  let obj = TLS_ALLOC.with(|alloc| unsafe {
    (*alloc.get()).immix.cursor = start;
    (*alloc.get()).immix.limit = limit;
    (*alloc.get()).immix.alloc_fast(size, align)
  });
  if let Some(obj) = obj {
    unsafe { init_object(obj, size, desc, epoch, false) };
    return obj;
  }

  crate::trap::rt_trap_oom(size, "rt_alloc: Immix hole too small");
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

    const TIMEOUT: Duration = Duration::from_secs(2);

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
        let obj = alloc_array(IMMIX_MAX_OBJECT_SIZE + 1024, 1);
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
