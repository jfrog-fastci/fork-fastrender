use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::config::{HeapConfig, HeapLimits};
use super::roots::RootHandle;
use super::roots::RememberedSet;
use super::roots::RootHandles;
use super::roots::RootSet;
use super::work_stack::WorkStack;
use super::weak::WeakHandle;
use super::weak::WeakHandles;
use super::ObjHeader;
use super::TypeDescriptor;
use crate::abi::RtShapeId;
use crate::array;
use crate::array::RtArrayHeader;
use crate::buffer::backing_store::{BackingStoreAllocator, GlobalBackingStoreAllocator};
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

const LOS_PAGE_SIZE: usize = 4096;

const OBJ_ALIGN: usize = mem::align_of::<ObjHeader>();

// Approximate metadata overhead for enforcing heap limits. This does not need to be exact, just
// stable/deterministic.
const METADATA_BASE_BYTES: usize = 4096;
const METADATA_PER_IMMIX_BLOCK: usize = 256;
const METADATA_PER_LOS_OBJECT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocKind {
  YoungPreferred,
  OldOnly,
  Pinned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocRequest {
  pub size: usize,
  pub align: usize,
  pub shape_id: RtShapeId,
  pub kind: Option<AllocKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocError {
  OutOfMemory,
}

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

#[derive(Debug, Clone, Copy)]
pub struct MajorCompactionConfig {
  pub enabled: bool,
  /// Candidate threshold based on Immix line occupancy.
  ///
  /// A block becomes a compaction candidate when:
  /// `live_lines / IMMIX_LINES_PER_BLOCK < max_live_ratio_percent / 100`.
  pub max_live_ratio_percent: u8,
  /// Avoid selecting extremely sparse blocks with only a handful of live lines.
  ///
  /// This also excludes fully-dead blocks (`live_lines == 0`), which are already
  /// reclaimed by normal mark-region sweeping.
  pub min_live_lines: usize,
}

impl Default for MajorCompactionConfig {
  fn default() -> Self {
    Self {
      enabled: false,
      max_live_ratio_percent: 25,
      min_live_lines: 1,
    }
  }
}

pub struct GcHeap {
  config: HeapConfig,
  limits: HeapLimits,

  pub(crate) nursery: nursery::NurserySpace,
  pub(crate) nursery_tlab: ThreadNursery,
  pub(crate) immix: ImmixSpace,
  pub(crate) los: LargeObjectSpace,
  weak_handles: WeakHandles,
  backing_store_alloc: Box<GlobalBackingStoreAllocator>,
  external_bytes: usize,
  finalizers: Vec<FinalizerEntry>,

  /// Current mark epoch (toggled on every major GC).
  pub(crate) mark_epoch: u8,

  pub(crate) major_compaction: MajorCompactionConfig,
  pub(crate) stats: GcStats,

  pub(crate) root_handles: RootHandles,
  pub(crate) work_stack: WorkStack,
}

#[derive(Clone, Copy)]
struct FinalizerEntry {
  obj: *mut u8,
  finalize: unsafe fn(&mut GcHeap, *mut u8),
}

// SAFETY: `GcHeap` is not safe for concurrent access, but it is safe to move between threads as
// long as callers provide external synchronization (e.g. stop-the-world GC coordination or a
// mutex). This enables using a heap behind a lock in process-wide singletons (like the string
// interner) without requiring every internal pointer type to be `Send`.
unsafe impl Send for GcHeap {}

/// RAII wrapper for a persistent GC root created by [`GcHeap::root_add`].
///
/// This is intended for runtime/host code that needs to keep an object alive across fallible
/// operations and wants to avoid leaking roots on early returns.
///
/// While the guard is alive it holds a mutable borrow of the [`GcHeap`]. For long-lived roots
/// stored in host state, prefer storing the returned [`RootHandle`] from [`GcHeap::root_add`]
/// directly.
#[must_use]
pub struct PersistentRoot<'a> {
  heap: &'a mut GcHeap,
  handle: RootHandle,
  // Prevent sending this guard across threads; it borrows the heap mutably and is intended for
  // short-lived rooting scopes.
  _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'a> PersistentRoot<'a> {
  pub fn new(heap: &'a mut GcHeap, value: *mut u8) -> Self {
    debug_assert!(!value.is_null(), "PersistentRoot cannot store a null pointer");
    let handle = heap.root_add(value);
    Self {
      heap,
      handle,
      _not_send_or_sync: PhantomData,
    }
  }

  #[inline]
  pub fn handle(&self) -> RootHandle {
    self.handle
  }

  #[inline]
  pub fn get(&self) -> Option<*mut u8> {
    self.heap.root_get(self.handle)
  }

  #[inline]
  pub fn set(&mut self, value: *mut u8) {
    self.heap.root_set(self.handle, value);
  }

  #[inline]
  pub fn heap(&self) -> &GcHeap {
    &*self.heap
  }

  #[inline]
  pub fn heap_mut(&mut self) -> &mut GcHeap {
    &mut *self.heap
  }
}

impl Drop for PersistentRoot<'_> {
  fn drop(&mut self) {
    self.heap.root_remove(self.handle);
  }
}

impl Default for GcHeap {
  fn default() -> Self {
    Self::new()
  }
}

impl GcHeap {
  pub fn new() -> Self {
    Self::with_config(HeapConfig::default(), HeapLimits::default())
  }

  pub fn with_nursery_size(nursery_size: usize) -> Self {
    Self {
      config: HeapConfig {
        nursery_size_bytes: nursery_size,
        ..HeapConfig::default()
      },
      limits: HeapLimits::default(),
      nursery: nursery::NurserySpace::new(nursery_size).expect("failed to reserve nursery space"),
      nursery_tlab: ThreadNursery::new(),
      immix: ImmixSpace::new(),
      los: LargeObjectSpace::new(),
      weak_handles: WeakHandles::new(),
      // `ArrayBuffer` backing stores live outside the GC heap but contribute to memory pressure.
      // We keep a per-heap backing-store allocator so `GcHeap::external_bytes()` can include their
      // total.
      backing_store_alloc: Box::new(GlobalBackingStoreAllocator::default()),
      external_bytes: 0,
      finalizers: Vec::new(),
      mark_epoch: 0,
      major_compaction: MajorCompactionConfig::default(),
      stats: GcStats::default(),
      root_handles: RootHandles::new(),
      work_stack: WorkStack::new(),
    }
  }

  pub fn with_config(config: HeapConfig, limits: HeapLimits) -> Self {
    let nursery =
      nursery::NurserySpace::new(config.nursery_size_bytes).expect("failed to reserve nursery space");

    Self {
      config,
      limits,
      nursery_tlab: ThreadNursery::new(),
      nursery,
      immix: ImmixSpace::new(),
      los: LargeObjectSpace::new(),
      weak_handles: WeakHandles::new(),
      backing_store_alloc: Box::new(GlobalBackingStoreAllocator::default()),
      external_bytes: 0,
      finalizers: Vec::new(),
      mark_epoch: 0,
      major_compaction: MajorCompactionConfig::default(),
      stats: GcStats::default(),
      root_handles: RootHandles::new(),
      work_stack: WorkStack::new(),
    }
  }

  pub fn config(&self) -> &HeapConfig {
    &self.config
  }

  pub fn limits(&self) -> &HeapLimits {
    &self.limits
  }

  pub fn stats(&self) -> &GcStats {
    &self.stats
  }

  pub fn major_compaction_config(&self) -> &MajorCompactionConfig {
    &self.major_compaction
  }

  pub fn major_compaction_config_mut(&mut self) -> &mut MajorCompactionConfig {
    &mut self.major_compaction
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

  /// Adds `value` to the heap's persistent root table and returns a guard that removes it on drop.
  #[inline]
  pub fn persistent_root(&mut self, value: *mut u8) -> PersistentRoot<'_> {
    PersistentRoot::new(self, value)
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

  /// Approximate total heap bytes for enforcing [`HeapLimits`].
  ///
  /// This is intentionally an estimate: it counts committed/reserved regions plus rough metadata
  /// overhead, so that OOM behavior is stable and deterministic.
  pub fn estimated_total_bytes(&self) -> usize {
    let nursery = self.nursery.size_bytes();
    let immix = self.immix.block_count() * IMMIX_BLOCK_SIZE;
    let los = self.los.committed_bytes();
    let meta = METADATA_BASE_BYTES
      + (self.immix.block_count() * METADATA_PER_IMMIX_BLOCK)
      + (self.los.object_count() * METADATA_PER_LOS_OBJECT);
    nursery + immix + los + meta
  }

  fn projected_total_bytes_with(
    &self,
    additional_immix_blocks: usize,
    additional_los_objects: usize,
    additional_los_committed_bytes: usize,
  ) -> usize {
    self
      .estimated_total_bytes()
      .saturating_add(additional_immix_blocks.saturating_mul(IMMIX_BLOCK_SIZE + METADATA_PER_IMMIX_BLOCK))
      .saturating_add(additional_los_objects.saturating_mul(METADATA_PER_LOS_OBJECT))
      .saturating_add(additional_los_committed_bytes)
  }

  pub fn alloc_object(
    &mut self,
    req: AllocRequest,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
  ) -> Result<*mut u8, AllocError> {
    if req.align == 0 || !req.align.is_power_of_two() {
      trap::rt_trap_invalid_arg("GcHeap::alloc_object: align must be a non-zero power of two");
    }

    let desc = type_desc_from_shape_id(req.shape_id);
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
    super::verify::register_type_descriptor(desc);

    if req.size != desc.size {
      trap::rt_trap_invalid_arg("GcHeap::alloc_object: size does not match registered shape");
    }

    let size = req.size;
    let align = req.align.max(OBJ_ALIGN);

    // If we're already above the hard cap, try a full collection before giving up.
    if self.estimated_total_bytes() > self.limits.max_heap_bytes {
      self.collect_major(roots, remembered)?;
      if self.estimated_total_bytes() > self.limits.max_heap_bytes {
        return Err(AllocError::OutOfMemory);
      }
    }

    if self.should_trigger_major() {
      self.collect_major(roots, remembered)?;
    }

    let kind = req.kind.unwrap_or(AllocKind::YoungPreferred);
    if kind == AllocKind::Pinned {
      if let Some(obj) = self.try_alloc_pinned(desc, size, align)? {
        return Ok(obj);
      }
      self.collect_major(roots, remembered)?;
      if let Some(obj) = self.try_alloc_pinned(desc, size, align)? {
        return Ok(obj);
      }
      return Err(AllocError::OutOfMemory);
    }

    let allow_nursery = matches!(kind, AllocKind::YoungPreferred);
    let wants_nursery =
      allow_nursery && size <= self.config.los_threshold_bytes && size <= self.config.nursery_size_bytes;

    if wants_nursery {
      if self.should_trigger_minor() {
        self.collect_minor(roots, remembered)?;
      }

      if let Some(obj) = self.try_alloc_young(desc, size, align) {
        return Ok(obj);
      }

      // Nursery allocation failed: trigger a minor collection and retry.
      self.collect_minor(roots, remembered)?;
      if let Some(obj) = self.try_alloc_young(desc, size, align) {
        return Ok(obj);
      }

      // If we still can't allocate, attempt a major collection and retry (also resets the nursery).
      self.collect_major(roots, remembered)?;
      if let Some(obj) = self.try_alloc_young(desc, size, align) {
        return Ok(obj);
      }
    }

    // Old/LOS allocation path.
    if let Some(obj) = self.try_alloc_old(desc, size, align, GrowMode::NoGrow)? {
      return Ok(obj);
    }
    self.collect_major(roots, remembered)?;
    if let Some(obj) = self.try_alloc_old(desc, size, align, GrowMode::NoGrow)? {
      return Ok(obj);
    }
    if let Some(obj) = self.try_alloc_old(desc, size, align, GrowMode::AllowGrow)? {
      return Ok(obj);
    }

    Err(AllocError::OutOfMemory)
  }

  pub fn alloc_young(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
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
      header.meta.store(0, Ordering::Relaxed);
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
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
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
       header.meta.store(0, Ordering::Relaxed);
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
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
    super::verify::register_type_descriptor(desc);

    let obj = self
      .alloc_old_raw(desc.size, OBJ_ALIGN)
      .unwrap_or_else(|_| panic!("old allocation out of space"));

    // SAFETY: The allocation is valid for `desc.size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, desc.size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta.store(0, Ordering::Relaxed);
      header.set_mark_epoch(self.mark_epoch);
    }

    obj
  }

  /// Allocate a pinned object in the large-object space (LOS), regardless of size.
  ///
  /// Pinned objects have a stable address across minor GC, major GC, and (future) compaction.
  /// They are still traced and reclaimed when unreachable.
  pub fn alloc_pinned(&mut self, desc: &'static TypeDescriptor) -> *mut u8 {
    self
      .try_alloc_pinned(desc, desc.size, OBJ_ALIGN)
      .and_then(|o| o.ok_or(AllocError::OutOfMemory))
      .unwrap_or_else(|_| panic!("pinned allocation out of space"))
  }

  pub(crate) fn alloc_old_raw(&mut self, size: usize, align: usize) -> Result<*mut u8, AllocError> {
    debug_assert!(align.is_power_of_two());
    self
      .try_alloc_old_raw(size, align, GrowMode::AllowGrow)
      .and_then(|o| o.ok_or(AllocError::OutOfMemory))
  }

  fn try_alloc_pinned(
    &mut self,
    desc: &'static TypeDescriptor,
    size: usize,
    align: usize,
  ) -> Result<Option<*mut u8>, AllocError> {
    debug_assert!(align.is_power_of_two());
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
    super::verify::register_type_descriptor(desc);

    let committed = round_up(size, LOS_PAGE_SIZE);
    let projected = self.projected_total_bytes_with(0, 1, committed);
    if projected > self.limits.max_heap_bytes {
      return Ok(None);
    }

    let obj = self.los.alloc(size, align);
    self.stats.bytes_allocated_old += size;

    // SAFETY: The allocation is valid for `size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta.store(0, Ordering::Relaxed);
      header.set_mark_epoch(self.mark_epoch);
      header.set_pinned(true);
    }

    Ok(Some(obj))
  }

  fn try_alloc_young(&mut self, desc: &'static TypeDescriptor, size: usize, align: usize) -> Option<*mut u8> {
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
    super::verify::register_type_descriptor(desc);

    let obj = self.nursery_tlab.alloc(size, align, &self.nursery)?;

    // SAFETY: The nursery allocation is valid for `size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta.store(0, Ordering::Relaxed);
      header.set_mark_epoch(self.mark_epoch);
    }

    self.stats.bytes_allocated_young += size;
    Some(obj)
  }

  fn try_alloc_old(
    &mut self,
    desc: &'static TypeDescriptor,
    size: usize,
    align: usize,
    grow: GrowMode,
  ) -> Result<Option<*mut u8>, AllocError> {
    #[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
    super::verify::register_type_descriptor(desc);

    let obj = self.try_alloc_old_raw(size, align, grow)?;
    let Some(obj) = obj else {
      return Ok(None);
    };

    // SAFETY: The allocation is valid for `size` bytes.
    unsafe {
      ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = desc as *const TypeDescriptor;
      header.meta.store(0, Ordering::Relaxed);
      header.set_mark_epoch(self.mark_epoch);
    }

    Ok(Some(obj))
  }

  fn try_alloc_old_raw(&mut self, size: usize, align: usize, grow: GrowMode) -> Result<Option<*mut u8>, AllocError> {
    debug_assert!(align.is_power_of_two());

    if size > self.config.los_threshold_bytes || size > IMMIX_MAX_OBJECT_SIZE {
      let committed = round_up(size, LOS_PAGE_SIZE);
      let projected = self.projected_total_bytes_with(0, 1, committed);
      if projected > self.limits.max_heap_bytes {
        return Ok(None);
      }

      let obj = self.los.alloc(size, align);
      self.stats.bytes_allocated_old += size;
      return Ok(Some(obj));
    }

    // Heuristic: if we don't have enough free space for the allocation, assume we'll need to grow
    // the Immix space (allocate a new block) and account for it up-front.
    let needs_grow = self.immix.block_count() == 0 || self.immix.free_bytes() < size;
    if needs_grow && grow == GrowMode::NoGrow {
      return Ok(None);
    }
    if needs_grow && grow == GrowMode::AllowGrow {
      let projected = self.projected_total_bytes_with(1, 0, 0);
      if projected > self.limits.max_heap_bytes {
        return Ok(None);
      }
    }

    let obj = self.immix.alloc_old(size, align);
    let Some(obj) = obj else {
      return Ok(None);
    };

    self.stats.bytes_allocated_old += size;
    Ok(Some(obj))
  }

  fn should_trigger_minor(&self) -> bool {
    let percent = self.config.minor_gc_nursery_used_percent as usize;
    if percent >= 100 {
      return false;
    }
    let used = self.nursery.allocated_bytes();
    let cap = self.nursery.size_bytes();
    used * 100 > cap * percent
  }

  fn should_trigger_major(&self) -> bool {
    let old_bytes = (self.immix.block_count() * IMMIX_BLOCK_SIZE).saturating_add(self.los.committed_bytes());
    old_bytes > self.config.major_gc_old_bytes_threshold
      || self.immix.block_count() > self.config.major_gc_old_blocks_threshold
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

  #[inline]
  pub(crate) fn is_valid_obj_ptr_for_tracing(&self, obj: *mut u8, allow_nursery: bool) -> bool {
    if obj.is_null() {
      return true;
    }

    let addr = obj as usize;
    if addr & (OBJ_ALIGN - 1) != 0 {
      return false;
    }

    if self.is_in_nursery(obj) {
      if !allow_nursery {
        return false;
      }

      let nursery_base = self.nursery.start() as usize;
      let nursery_alloc_end = nursery_base.saturating_add(self.nursery.allocated_bytes());
      return addr < nursery_alloc_end;
    }

    self.is_in_immix(obj) || self.is_in_los(obj)
  }

  pub fn immix_block_count(&self) -> usize {
    self.immix.block_count()
  }

  pub fn immix_free_block_count(&self) -> usize {
    self.immix.free_block_count()
  }

  pub fn immix_free_bytes(&self) -> usize {
    self.immix.free_bytes()
  }

  pub fn los_object_count(&self) -> usize {
    self.los.object_count()
  }

  #[inline]
  pub(crate) fn backing_store_allocator(&self) -> &GlobalBackingStoreAllocator {
    &*self.backing_store_alloc
  }

  #[inline]
  pub fn add_external_bytes(&mut self, bytes: usize) {
    self.external_bytes = self.external_bytes.saturating_add(bytes);
  }

  #[inline]
  pub fn sub_external_bytes(&mut self, bytes: usize) {
    debug_assert!(
      self.external_bytes >= bytes,
      "external_bytes underflow (tracked={}, sub={})",
      self.external_bytes,
      bytes
    );
    self.external_bytes = self.external_bytes.saturating_sub(bytes);
  }

  #[inline]
  pub fn external_bytes(&self) -> usize {
    self
      .external_bytes
      .saturating_add(self.backing_store_alloc.external_bytes())
  }

  /// Register a finalizer for a GC-managed object.
  ///
  /// Finalizers run exactly once when the object becomes unreachable:
  /// - During minor GC for dead nursery objects (before nursery reset).
  /// - During major GC for dead old/LOS objects (after marking, before sweeping).
  ///
  /// The finalizer must not assume the object has a stable address across GCs; it receives the
  /// current object base pointer at the time it runs.
  pub fn register_finalizer(&mut self, obj: *mut u8, finalize: unsafe fn(&mut GcHeap, *mut u8)) {
    self.finalizers.push(FinalizerEntry { obj, finalize });
  }

  pub(crate) fn process_finalizers_minor(&mut self) {
    let mut i = 0usize;
    while i < self.finalizers.len() {
      let obj = self.finalizers[i].obj;
      if obj.is_null() {
        self.finalizers.swap_remove(i);
        continue;
      }

      if self.nursery.contains(obj) {
        // SAFETY: `obj` is expected to point to the start of a nursery object.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            self.finalizers[i].obj = header.forwarding_ptr();
            i += 1;
          } else {
            let entry = self.finalizers.swap_remove(i);
            (entry.finalize)(self, obj);
          }
        }
      } else {
        i += 1;
      }
    }
  }

  pub(crate) fn process_finalizers_major(&mut self, epoch: u8) {
    let mut i = 0usize;
    while i < self.finalizers.len() {
      let mut obj = self.finalizers[i].obj;
      if obj.is_null() {
        self.finalizers.swap_remove(i);
        continue;
      }

      // Major GC should not see nursery pointers (it runs a minor GC first), but handle them
      // defensively.
      if self.nursery.contains(obj) {
        // SAFETY: `obj` points into nursery memory.
        unsafe {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
          } else {
            let entry = self.finalizers.swap_remove(i);
            (entry.finalize)(self, obj);
            continue;
          }
        }
      }

      // If the object isn't in this heap anymore (e.g. swept large object), drop the finalizer
      // record. This keeps us from dereferencing stale pointers if clients register finalizers on
      // arbitrary pointers.
      if !self.immix.contains(obj) && !self.los.contains(obj) {
        self.finalizers.swap_remove(i);
        continue;
      }

      // Follow forwarding pointers (used by nursery evacuation today, and by potential future major
      // GC compaction).
      unsafe {
        loop {
          let header = &*(obj as *const ObjHeader);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
          } else {
            break;
          }
        }
      }

      self.finalizers[i].obj = obj;

      // SAFETY: `obj` points to a heap object header.
      let marked = unsafe { (&*(obj as *const ObjHeader)).is_marked(epoch) };
      if marked {
        i += 1;
      } else {
        let entry = self.finalizers.swap_remove(i);
        unsafe { (entry.finalize)(self, obj) };
      }
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowMode {
  NoGrow,
  AllowGrow,
}

fn type_desc_from_shape_id(shape_id: RtShapeId) -> &'static TypeDescriptor {
  crate::shape_table::lookup_type_descriptor(shape_id)
}

#[inline]
fn align_up(n: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (n + (align - 1)) & !(align - 1)
}

#[inline]
fn round_up(n: usize, m: usize) -> usize {
  align_up(n, m)
}
