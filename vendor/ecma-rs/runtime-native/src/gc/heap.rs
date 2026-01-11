use std::alloc::handle_alloc_error;
use std::alloc::Layout;
use std::mem;
use std::ptr;
use std::time::Duration;

use super::roots::RootHandle;
use super::roots::RootHandles;
use super::ObjHeader;
use super::TypeDescriptor;
use super::weak::WeakHandle;
use super::weak::WeakHandles;
use crate::array;
use crate::array::RtArrayHeader;
use crate::trap;
use crate::immix;
use crate::immix::ImmixSpace;
use crate::los::LargeObjectSpace;
use crate::nursery;
use crate::nursery::ThreadNursery;

/// Immix block size in bytes.
pub const IMMIX_BLOCK_SIZE: usize = immix::BLOCK_SIZE;

/// Immix line size in bytes.
pub const IMMIX_LINE_SIZE: usize = immix::LINE_SIZE;

pub const IMMIX_LINES_PER_BLOCK: usize = immix::LINES_PER_BLOCK;

/// Maximum object size that is eligible for Immix allocation.
pub const IMMIX_MAX_OBJECT_SIZE: usize = IMMIX_BLOCK_SIZE / 2;

const OBJ_ALIGN: usize = mem::align_of::<ObjHeader>();

#[derive(Debug, Default)]
pub struct GcStats {
  pub minor_collections: usize,
  pub major_collections: usize,
  pub bytes_allocated_young: usize,
  pub bytes_allocated_old: usize,
  pub last_major_live_bytes: usize,
  pub last_minor_pause: Duration,
  pub last_major_pause: Duration,
  pub total_minor_pause: Duration,
  pub total_major_pause: Duration,
}

pub struct GcHeap {
  pub(crate) nursery: nursery::NurserySpace,
  pub(crate) nursery_tlab: ThreadNursery,
  pub(crate) immix: ImmixSpace,
  pub(crate) los: LargeObjectSpace,
  weak_handles: WeakHandles,

  /// Current mark epoch (toggled on every major GC).
  pub(crate) mark_epoch: u8,

  pub(crate) stats: GcStats,

  pub(crate) root_handles: RootHandles,
}

// SAFETY: `GcHeap` is not safe for concurrent access, but it is safe to move between threads as
// long as callers provide external synchronization (e.g. stop-the-world GC coordination or a
// mutex). This enables using a heap behind a lock in process-wide singletons (like the string
// interner) without requiring every internal pointer type to be `Send`.
unsafe impl Send for GcHeap {}

impl Default for GcHeap {
  fn default() -> Self {
    Self::new()
  }
}

impl GcHeap {
  pub fn new() -> Self {
    Self::with_nursery_size(nursery::DEFAULT_NURSERY_SIZE_BYTES)
  }

  pub fn with_nursery_size(nursery_size: usize) -> Self {
    Self {
      nursery: nursery::NurserySpace::new(nursery_size).expect("failed to reserve nursery space"),
      nursery_tlab: ThreadNursery::new(),
      immix: ImmixSpace::new(),
      los: LargeObjectSpace::new(),
      weak_handles: WeakHandles::new(),
      mark_epoch: 0,
      stats: GcStats::default(),
      root_handles: RootHandles::new(),
    }
  }

  pub fn stats(&self) -> &GcStats {
    &self.stats
  }

  pub fn nursery_stats(&self) -> nursery::NurseryStats {
    self.nursery.stats()
  }

  pub fn nursery_reserved_bytes(&self) -> usize {
    self.nursery.size_bytes()
  }

  pub fn nursery_allocated_bytes(&self) -> usize {
    self.nursery.allocated_bytes()
  }

  pub fn weak_add(&mut self, ptr: *mut u8) -> WeakHandle {
    self.weak_handles.weak_add(ptr)
  }

  pub fn weak_get(&self, handle: WeakHandle) -> Option<*mut u8> {
    self.weak_handles.weak_get(handle)
  }

  pub fn weak_set(&mut self, handle: WeakHandle, ptr: *mut u8) {
    self.weak_handles.weak_set(handle, ptr);
  }

  pub fn weak_remove(&mut self, handle: WeakHandle) {
    self.weak_handles.weak_remove(handle);
  }

  #[inline]
  pub fn root_add(&mut self, value: *mut u8) -> RootHandle {
    self.root_handles.root_add(value)
  }

  #[inline]
  pub fn root_get(&self, h: RootHandle) -> Option<*mut u8> {
    self.root_handles.root_get(h)
  }

  #[inline]
  pub fn root_set(&mut self, h: RootHandle, value: *mut u8) {
    self.root_handles.root_set(h, value);
  }

  #[inline]
  pub fn root_remove(&mut self, h: RootHandle) {
    self.root_handles.root_remove(h);
  }

  pub(crate) fn process_weak_handles_minor(&mut self) {
    let nursery = &self.nursery;

    self.weak_handles.for_each_slot_mut(|slot| {
      let obj = *slot;
      if obj.is_null() {
        return;
      }

      if nursery.contains(obj) {
        // SAFETY: `obj` is expected to point at the start of a nursery object.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            *slot = header.forwarding_ptr();
          } else {
            *slot = ptr::null_mut();
          }
        }
      }
    });
  }

  pub(crate) fn process_weak_handles_major(&mut self, epoch: u8) {
    let nursery = &self.nursery;
    let immix = &self.immix;
    let los = &self.los;

    self.weak_handles.for_each_slot_mut(|slot| {
      let mut obj = *slot;
      if obj.is_null() {
        return;
      }

      if nursery.contains(obj) {
        // Major GC should not see nursery pointers (it runs a minor GC first),
        // but handle them defensively.
        // SAFETY: `obj` is expected to point at the start of a nursery object.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
          } else {
            *slot = ptr::null_mut();
            return;
          }
        }
      }

      // If the referent isn't in this heap anymore (e.g. swept large object),
      // clear the slot. This avoids dereferencing stale pointers.
      if !immix.contains(obj) && !los.contains(obj) {
        *slot = ptr::null_mut();
        return;
      }

      // Follow forwarding pointers (used by nursery evacuation today, and by
      // potential future major GC compaction).
      // SAFETY: `obj` is expected to point at the start of a heap object.
      unsafe {
        loop {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
          } else {
            break;
          }
        }

        let header = &*(obj as *const ObjHeader);
        if header.is_marked(epoch) {
          *slot = obj;
        } else {
          *slot = ptr::null_mut();
        }
      }
    });
  }

  pub fn alloc_young(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    super::verify::register_type_descriptor(desc);

    let obj = self
      .nursery_tlab
      .alloc(desc.size, OBJ_ALIGN, &self.nursery)
      .expect("nursery out of space");

    // Ensure pointer slots start out as null so tracing never sees uninitialized garbage.
    // SAFETY: The nursery allocation is valid for `desc.size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, desc.size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta = 0;
      header.set_mark_epoch(self.mark_epoch);
    }

    self.stats.bytes_allocated_young += desc.size;
    obj
  }

  /// Allocate a GC-managed array object in the nursery (young generation).
  ///
  /// The returned pointer is the **object base pointer** (start of [`ObjHeader`]), and can be cast
  /// to [`RtArrayHeader`].
  ///
  /// `elem_size` uses the same encoding as the public `rt_alloc_array` ABI: the high bit is
  /// reserved for `RT_ARRAY_ELEM_PTR_FLAG` (see [`crate::array::decode_rt_array_elem_size`]).
  pub fn alloc_array_young(&mut self, len: usize, elem_size: usize) -> *mut u8 {
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    super::verify::register_type_descriptor(&array::RT_ARRAY_TYPE_DESC);

    let Some(spec) = array::decode_rt_array_elem_size(elem_size) else {
      trap::rt_trap_invalid_arg("invalid rt_alloc_array elem_size");
    };
    let size = array::checked_total_bytes(len, spec.elem_size)
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

    let obj = self
      .nursery_tlab
      .alloc(size, OBJ_ALIGN, &self.nursery)
      .expect("nursery out of space");

    // SAFETY: The nursery allocation is valid for `size` bytes.
    unsafe {
      // Ensure all pointer slots start out as null so tracing never sees uninitialized garbage.
      ptr::write_bytes(obj, 0, size);

      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor;
      header.meta = 0;
      header.set_mark_epoch(self.mark_epoch);

      let arr = &mut *(obj as *mut RtArrayHeader);
      arr.len = len;
      arr.elem_size = spec.elem_size as u32;
      arr.elem_flags = spec.elem_flags;
    }

    self.stats.bytes_allocated_young += size;
    obj
  }

  pub fn alloc_old(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    super::verify::register_type_descriptor(desc);

    let obj = self.alloc_old_raw(desc.size, OBJ_ALIGN);

    // SAFETY: The allocation is valid for `desc.size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, desc.size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta = 0;
      header.set_mark_epoch(self.mark_epoch);
    }

    obj
  }

  /// Allocate a pinned object in the large-object space (LOS), regardless of size.
  ///
  /// Pinned objects have a stable address across minor GC, major GC, and (future) compaction.
  /// They are still traced and reclaimed when unreachable.
  pub fn alloc_pinned(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    super::verify::register_type_descriptor(desc);

    let obj = self.los.alloc(desc.size, OBJ_ALIGN);
    self.stats.bytes_allocated_old += desc.size;

    // SAFETY: The allocation is valid for `desc.size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, desc.size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta = 0;
      header.set_mark_epoch(self.mark_epoch);
      header.set_pinned(true);
    }

    obj
  }

  pub(crate) fn alloc_old_raw(&mut self, size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    let obj = if size > IMMIX_MAX_OBJECT_SIZE {
      self.los.alloc(size, align)
    } else {
      self
        .immix
        .alloc_old(size, align)
        .unwrap_or_else(|| handle_alloc_error(Layout::from_size_align(size, align).unwrap()))
    };

    self.stats.bytes_allocated_old += size;
    obj
  }

  pub fn is_in_nursery(&self, obj: *mut u8) -> bool {
    self.nursery.contains(obj)
  }

  pub fn is_in_immix(&self, obj: *mut u8) -> bool {
    self.immix.contains(obj)
  }

  pub fn is_in_los(&self, obj: *mut u8) -> bool {
    self.los.contains(obj)
  }

  pub fn immix_block_count(&self) -> usize {
    self.immix.block_count()
  }

  pub fn immix_free_bytes(&self) -> usize {
    self.immix.free_bytes()
  }

  pub fn los_object_count(&self) -> usize {
    self.los.object_count()
  }
}
