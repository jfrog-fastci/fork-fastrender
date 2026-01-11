//! Runtime allocation fast paths.
//!
//! This module backs the `rt_alloc*` C ABI exports with:
//! - per-thread nursery TLAB bump allocation (`nursery::ThreadNursery`)
//! - per-thread Immix bump cursor within a reserved hole (`ImmixCursor`)
//!
//! The hot path performs no global locking.
//! Slow paths (Immix hole reservation, LOS allocation, GC) are serialized.
#![allow(dead_code)]

use crate::abi::RtShapeId;
use crate::array;
use crate::gc::heap::IMMIX_BLOCK_SIZE;
use crate::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use crate::gc::{ObjHeader, TypeDescriptor, YOUNG_SPACE, OBJ_ALIGN};
use crate::immix::LINE_SIZE;
use crate::nursery::{NurserySpace, ThreadNursery};
use crate::threading::ThreadId;
use crate::shape_table;
use parking_lot::Mutex;
use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

#[inline(always)]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
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
}

impl ThreadAlloc {
  pub const fn new() -> Self {
    Self {
      nursery: ThreadNursery::new(),
      immix: ImmixCursor::new(),
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
  heap_lock: Mutex<()>,
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
      heap_lock: Mutex::new(()),
    }
  })
}

#[inline]
fn nursery(global: &GlobalHeap) -> &NurserySpace {
  // SAFETY: `global.heap` points to a leaked `GcHeap` that outlives the process.
  unsafe { &(*(global.heap as *mut crate::gc::GcHeap)).nursery }
}

#[inline]
fn current_mark_epoch(global: &GlobalHeap) -> u8 {
  // SAFETY: `mark_epoch` is only mutated during stop-the-world GC. Mutator threads only read it
  // outside STW.
  unsafe { (*(global.heap as *mut crate::gc::GcHeap)).mark_epoch }
}

fn with_heap_lock_mutator<R>(f: impl FnOnce(&mut crate::gc::GcHeap) -> R) -> R {
  let global = global_heap();

  loop {
    if let Some(_guard) = global.heap_lock.try_lock() {
      // SAFETY: `_guard` serializes access to the non-thread-safe parts of `GcHeap`.
      let heap = unsafe { &mut *(global.heap as *mut crate::gc::GcHeap) };
      return f(heap);
    }

    // Avoid deadlocking the stop-the-world GC: while waiting for the lock, keep polling safepoints.
    crate::threading::safepoint_poll();
    std::hint::spin_loop();
  }
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
    (*alloc.get()).nursery.alloc(size, align, nursery(global))
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
    let obj = with_heap_lock_mutator(|heap| heap.los.alloc(size, align));
    unsafe { init_object(obj, size, &array::RT_ARRAY_TYPE_DESC, epoch, false) };
    unsafe {
      let arr = &mut *(obj as *mut array::RtArrayHeader);
      arr.len = len;
      arr.elem_size = spec.elem_size as u32;
      arr.elem_flags = spec.elem_flags;
    }
    if should_install_card_table {
      let card_table = crate::gc::alloc_card_table(size);
      if !card_table.is_null() {
        // SAFETY: `card_table` was allocated with `alloc_card_table`, which
        // guarantees the alignment + length required by `ObjHeader`.
        unsafe {
          (&mut *(obj as *mut ObjHeader)).set_card_table_ptr(card_table);
        }
      }
    }
    return obj;
  }

  if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe {
    (*alloc.get()).nursery.alloc(size, align, nursery(global))
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

  let obj = alloc_old(size, align, &array::RT_ARRAY_TYPE_DESC, epoch);
  unsafe {
    let arr = &mut *(obj as *mut array::RtArrayHeader);
    arr.len = len;
    arr.elem_size = spec.elem_size as u32;
    arr.elem_flags = spec.elem_flags;
  }
  if should_install_card_table {
    let card_table = crate::gc::alloc_card_table(size);
    if !card_table.is_null() {
      // SAFETY: `card_table` was allocated with `alloc_card_table`, which
      // guarantees the alignment + length required by `ObjHeader`.
      unsafe {
        (&mut *(obj as *mut ObjHeader)).set_card_table_ptr(card_table);
      }
    }
  }
  obj
}

pub(crate) fn alloc_pinned(size: usize, shape: RtShapeId) -> *mut u8 {
  ensure_thread_registered_for_alloc();
  crate::threading::safepoint_poll();

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
  if let Some(obj) = TLS_ALLOC.with(|alloc| unsafe { (*alloc.get()).immix.alloc_fast(size, align) }) {
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
