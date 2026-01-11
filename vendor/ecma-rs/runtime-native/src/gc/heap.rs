use std::alloc::alloc_zeroed;
use std::alloc::dealloc;
use std::alloc::handle_alloc_error;
use std::alloc::Layout;
use std::mem;
use std::ptr;
use std::ptr::NonNull;

use super::align_up;
use super::ObjHeader;
use super::TypeDescriptor;
use super::weak::WeakHandle;
use super::weak::WeakHandles;
use crate::nursery;
use crate::nursery::ThreadNursery;

/// Immix block size in bytes.
pub const IMMIX_BLOCK_SIZE: usize = 32 * 1024;

/// Immix line size in bytes.
pub const IMMIX_LINE_SIZE: usize = 256;

pub const IMMIX_LINES_PER_BLOCK: usize = IMMIX_BLOCK_SIZE / IMMIX_LINE_SIZE;

/// Maximum object size that is eligible for Immix allocation.
pub const IMMIX_MAX_OBJECT_SIZE: usize = IMMIX_BLOCK_SIZE / 2;

const OBJ_ALIGN: usize = mem::align_of::<ObjHeader>();

#[derive(Debug, Default)]
pub struct GcStats {
  pub minor_collections: usize,
  pub major_collections: usize,
  pub bytes_allocated_young: usize,
  pub bytes_allocated_old: usize,
}

struct RawMemory {
  ptr: NonNull<u8>,
  layout: Layout,
}

impl RawMemory {
  fn new_zeroed(size: usize, align: usize) -> Self {
    let layout = Layout::from_size_align(size, align).expect("invalid allocation layout");
    // SAFETY: `layout` is valid.
    let ptr = unsafe { alloc_zeroed(layout) };
    let ptr = match NonNull::new(ptr) {
      Some(p) => p,
      None => handle_alloc_error(layout),
    };
    Self { ptr, layout }
  }

  #[inline]
  fn as_ptr(&self) -> *mut u8 {
    self.ptr.as_ptr()
  }
}

impl Drop for RawMemory {
  fn drop(&mut self) {
    // SAFETY: The pointer was allocated with this `layout`.
    unsafe {
      dealloc(self.ptr.as_ptr(), self.layout);
    }
  }
}

struct ImmixBlock {
  mem: RawMemory,
  line_mark: [u8; IMMIX_LINES_PER_BLOCK],
  free_ranges: Vec<(usize, usize)>,
}

impl ImmixBlock {
  fn new() -> Self {
    Self {
      mem: RawMemory::new_zeroed(IMMIX_BLOCK_SIZE, IMMIX_LINE_SIZE),
      line_mark: [0; IMMIX_LINES_PER_BLOCK],
      free_ranges: vec![(0, IMMIX_BLOCK_SIZE)],
    }
  }

  #[inline]
  fn base(&self) -> usize {
    self.mem.as_ptr() as usize
  }

  fn contains(&self, ptr: *mut u8) -> bool {
    let p = ptr as usize;
    let base = self.base();
    p >= base && p < base + IMMIX_BLOCK_SIZE
  }

  fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
    debug_assert!(size <= IMMIX_BLOCK_SIZE);
    let base = self.base();

    for idx in 0..self.free_ranges.len() {
      let (range_start, range_end) = self.free_ranges[idx];

      let abs_start = base + range_start;
      let abs_alloc_start = align_up(abs_start, align);
      let alloc_start = abs_alloc_start - base;
      let alloc_end = alloc_start.checked_add(size)?;

      if alloc_end <= range_end {
        let mut replacement = Vec::new();
        if range_start < alloc_start {
          replacement.push((range_start, alloc_start));
        }
        if alloc_end < range_end {
          replacement.push((alloc_end, range_end));
        }

        self.free_ranges.splice(idx..=idx, replacement);
        return Some((base + alloc_start) as *mut u8);
      }
    }

    None
  }

  fn clear_line_marks(&mut self) {
    self.line_mark.fill(0);
  }

  fn set_lines_for_live_object(&mut self, obj: *mut u8, size: usize) {
    let start = (obj as usize).wrapping_sub(self.base());
    let end = start + size;
    debug_assert!(end <= IMMIX_BLOCK_SIZE);

    let start_line = start / IMMIX_LINE_SIZE;
    let end_line = (end - 1) / IMMIX_LINE_SIZE;

    for line in start_line..=end_line {
      self.line_mark[line] = 1;
    }
  }

  fn rebuild_free_ranges_from_marks(&mut self) {
    self.free_ranges.clear();

    let mut run_start: Option<usize> = None;
    for line in 0..IMMIX_LINES_PER_BLOCK {
      let is_free = self.line_mark[line] == 0;
      match (run_start, is_free) {
        (None, true) => run_start = Some(line),
        (Some(_), true) => {}
        (Some(start), false) => {
          self
            .free_ranges
            .push((start * IMMIX_LINE_SIZE, line * IMMIX_LINE_SIZE));
          run_start = None;
        }
        (None, false) => {}
      }
    }

    if let Some(start) = run_start {
      self.free_ranges.push((start * IMMIX_LINE_SIZE, IMMIX_BLOCK_SIZE));
    }
  }

  fn free_bytes(&self) -> usize {
    self
      .free_ranges
      .iter()
      .map(|(start, end)| end - start)
      .sum()
  }
}

pub(crate) struct ImmixSpace {
  blocks: Vec<ImmixBlock>,
}

impl ImmixSpace {
  fn new() -> Self {
    Self { blocks: Vec::new() }
  }

  fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
    debug_assert!(size <= IMMIX_MAX_OBJECT_SIZE);

    for block in &mut self.blocks {
      if let Some(ptr) = block.alloc(size, align) {
        return ptr;
      }
    }

    let mut block = ImmixBlock::new();
    let ptr = block
      .alloc(size, align)
      .expect("fresh Immix block must have enough space");
    self.blocks.push(block);
    ptr
  }

  fn contains(&self, ptr: *mut u8) -> bool {
    self.blocks.iter().any(|b| b.contains(ptr))
  }

  pub(crate) fn clear_line_marks(&mut self) {
    for block in &mut self.blocks {
      block.clear_line_marks();
    }
  }

  pub(crate) fn set_lines_for_live_object(&mut self, obj: *mut u8, size: usize) {
    for block in &mut self.blocks {
      if block.contains(obj) {
        block.set_lines_for_live_object(obj, size);
        return;
      }
    }
    debug_assert!(false, "object not in ImmixSpace");
  }

  pub(crate) fn finalize_after_marking(&mut self) {
    for block in &mut self.blocks {
      block.rebuild_free_ranges_from_marks();
    }
  }

  fn block_count(&self) -> usize {
    self.blocks.len()
  }

  fn free_bytes(&self) -> usize {
    self.blocks.iter().map(|b| b.free_bytes()).sum()
  }
}

struct LargeObject {
  mem: RawMemory,
}

pub(crate) struct LargeObjectSpace {
  objects: Vec<LargeObject>,
}

impl LargeObjectSpace {
  fn new() -> Self {
    Self { objects: Vec::new() }
  }

  fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
    let mem = RawMemory::new_zeroed(size, align);
    let ptr = mem.as_ptr();
    self.objects.push(LargeObject { mem });
    ptr
  }

  fn contains(&self, ptr: *mut u8) -> bool {
    self.objects.iter().any(|o| o.mem.as_ptr() == ptr)
  }

  pub(crate) fn sweep(&mut self, current_epoch: u8) {
    self.objects.retain(|obj| {
      // SAFETY: The object is a valid allocation and always begins with ObjHeader.
      let header = unsafe { &*(obj.mem.as_ptr() as *const ObjHeader) };
      header.is_marked(current_epoch)
    });
  }

  fn object_count(&self) -> usize {
    self.objects.len()
  }
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
}

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
    }
  }

  pub fn stats(&self) -> &GcStats {
    &self.stats
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

  pub fn alloc_old(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
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
      self.immix.alloc(size, align)
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
