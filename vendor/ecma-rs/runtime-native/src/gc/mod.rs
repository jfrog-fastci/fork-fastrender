use std::mem;
use std::slice;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::array;
use crate::trap;

pub mod config;
pub mod heap;
pub mod roots;
pub mod handle_table;
pub mod shadow_stack;
pub mod weak;
mod young;
mod cards;

mod evacuate;
mod mark;
mod keep_alive;
mod work_stack;

pub use config::{HeapConfig, HeapLimits};
pub use heap::{AllocError, AllocKind, AllocRequest, GcHeap, PersistentRoot};
pub use keep_alive::keep_alive_gc_ref;
pub use handle_table::{HandleId, HandleTable, OwnedGcHandle, PersistentHandle};
pub use roots::RememberedSet;
pub use roots::RootHandle;
pub use roots::RootSet;
pub use roots::RootStack;
pub use shadow_stack::GcRawPtr;
pub use shadow_stack::RootScope;
pub use shadow_stack::ShadowStack;
pub use roots::SimpleRememberedSet;
pub use weak::register_weak_cleanup;
pub use weak::WeakHandle;
pub use weak::WeakHandles;
pub use young::YoungSpace;

pub(crate) use young::YOUNG_SPACE;
#[cfg(any(debug_assertions, feature = "gc_debug"))]
pub(crate) use verify::register_type_descriptor_ptr;

/// Align `value` up to the next multiple of `align` (power-of-two).
///
/// This is shared by a few fixed-size object layout helpers (e.g. the string
/// interner) that need a stable, aligned `TypeDescriptor::size`.
#[inline]
pub(crate) fn align_up(value: usize, align: usize) -> usize {
  if align == 0 || !align.is_power_of_two() {
    trap::rt_trap_invalid_arg("align_up: align must be a non-zero power of two");
  }
  debug_assert!(align.is_power_of_two());
  let mask = align - 1;
  value
    .checked_add(mask)
    .map(|v| v & !mask)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("align_up overflow"))
}

/// Number of bytes covered by a single card in a per-object card table.
pub const CARD_SIZE: usize = 512;

/// Minimum array payload size (in bytes) to enable a per-object card table.
///
/// Card tables are only installed for **old-generation pointer arrays** whose
/// element payload is at least this large. For smaller arrays, rescanning the
/// full object is typically cheaper than maintaining card metadata.
///
/// See `docs/write_barrier.md` for the benchmark-driven default heuristic.
pub const CARD_TABLE_MIN_BYTES: usize = 8 * CARD_SIZE;

/// Return the number of `AtomicU64` words required for a per-object card table
/// covering an object of `obj_size` bytes.
///
/// Card table bitset layout:
/// - `cards = ceil(obj_size / CARD_SIZE)` bits
/// - `words = ceil(cards / 64)` 64-bit words
#[inline]
pub(crate) fn card_table_word_count(obj_size: usize) -> usize {
  // `obj_size` should always be > 0, but be defensive in case callers route a
  // malformed descriptor here.
  let cards = obj_size.div_ceil(CARD_SIZE).max(1);
  cards.div_ceil(64).max(1)
}

/// Clear the card table bitset for `obj` if one is installed.
///
/// # Safety
/// `obj` must be a valid GC object base pointer.
pub(crate) unsafe fn clear_card_table_for_obj(obj: *mut u8) {
  debug_assert!(!obj.is_null());
  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if card_table.is_null() {
    return;
  }

  let size = obj_size(obj);
  let words = card_table_word_count(size);
  for i in 0..words {
    (*card_table.add(i)).store(0, Ordering::Release);
  }
}
/// Object header that prefixes every GC-managed allocation.
///
/// # Layout
/// The object pointer is a raw `*mut u8` that points at the start of this header.
/// The header is followed by the object's payload as described by its
/// [`TypeDescriptor`].
#[repr(C)]
pub struct ObjHeader {
  pub(crate) type_desc: *const TypeDescriptor,
  pub(crate) meta: AtomicUsize,
}

pub const OBJ_HEADER_SIZE: usize = mem::size_of::<ObjHeader>();
/// Minimum alignment (in bytes) guaranteed for all GC-managed object base pointers.
///
/// Codegen may assume `rt_alloc` / `rt_alloc_pinned` return pointers aligned to at least this.
pub const OBJ_ALIGN: usize = if mem::align_of::<ObjHeader>() > 16 {
  mem::align_of::<ObjHeader>()
} else {
  16
};

// `meta` layout:
// - bit 0: forwarded bit (nursery only)
// - bit 1: mark epoch (0/1)
// - bit 2: remembered bit (old object has an old->young pointer)
// - bit 3: pinned bit (object must not be moved by compaction/evacuation)
const META_FORWARDED: usize = 1;
const META_MARK_SHIFT: usize = 1;
const META_MARK_MASK: usize = 1 << META_MARK_SHIFT;
const META_REMEMBERED: usize = 1 << 2;
const META_PINNED: usize = 1 << 3;
const META_FLAGS_MASK: usize = META_FORWARDED | META_MARK_MASK | META_REMEMBERED | META_PINNED;

impl ObjHeader {
  #[inline]
  pub(crate) unsafe fn type_desc(&self) -> &TypeDescriptor {
    debug_assert!(!self.type_desc.is_null());
    &*self.type_desc
  }

  #[inline]
  pub(crate) fn is_forwarded(&self) -> bool {
    (self.meta.load(Ordering::Acquire) & META_FORWARDED) != 0
  }

  #[inline]
  pub(crate) fn forwarding_ptr(&self) -> *mut u8 {
    debug_assert!(self.is_forwarded());
    (self.meta.load(Ordering::Acquire) & !META_FORWARDED) as *mut u8
  }

  #[inline]
  pub(crate) fn set_forwarding_ptr(&mut self, new_location: *mut u8) {
    debug_assert!((new_location as usize & META_FORWARDED) == 0);
    assert!(
      !self.is_pinned(),
      "attempted to evacuate/forward a pinned object"
    );
    self.meta.store((new_location as usize) | META_FORWARDED, Ordering::Release);
  }

  #[inline]
  pub fn is_remembered(&self) -> bool {
    // When the header is in the "forwarded" state, `meta` is a tagged forwarding pointer, so any
    // other bit tests are meaningless.
    !self.is_forwarded() && (self.meta.load(Ordering::Acquire) & META_REMEMBERED) != 0
  }

  /// Debug/test helper: clear the per-object `REMEMBERED` bit.
  ///
  /// This is intentionally not part of the stable runtime ABI; it exists so
  /// integration tests can model remembered-set rebuild behavior without
  /// depending on a process-global remembered set.
  #[doc(hidden)]
  pub fn clear_remembered_for_tests(&self) {
    if self.is_forwarded() {
      return;
    }
    self.meta.fetch_and(!META_REMEMBERED, Ordering::Release);
  }

  #[inline]
  pub fn is_pinned(&self) -> bool {
    // When the header is in the "forwarded" state, `meta` is a tagged forwarding pointer, so any
    // other bit tests are meaningless.
    !self.is_forwarded() && (self.meta.load(Ordering::Acquire) & META_PINNED) != 0
  }

  #[inline]
  pub(crate) fn set_remembered_idempotent(&self) -> bool {
    if self.is_forwarded() {
      return false;
    }
    let prev = self.meta.fetch_or(META_REMEMBERED, Ordering::AcqRel);
    (prev & META_REMEMBERED) == 0
  }

  #[inline]
  pub(crate) fn clear_remembered_idempotent(&self) {
    if self.is_forwarded() {
      return;
    }
    self.meta.fetch_and(!META_REMEMBERED, Ordering::AcqRel);
  }

  #[inline]
  pub(crate) fn set_remembered(&mut self, remembered: bool) {
    if self.is_forwarded() {
      return;
    }
    if remembered {
      self.meta.fetch_or(META_REMEMBERED, Ordering::Release);
    } else {
      self.meta.fetch_and(!META_REMEMBERED, Ordering::Release);
    }
  }

  #[inline]
  pub(crate) fn set_pinned(&mut self, pinned: bool) {
    debug_assert!(!self.is_forwarded(), "pinned objects must not be forwarded");
    if self.is_forwarded() {
      return;
    }
    if pinned {
      self.meta.fetch_or(META_PINNED, Ordering::Release);
    } else {
      self.meta.fetch_and(!META_PINNED, Ordering::Release);
    }
  }

  #[inline]
  pub(crate) fn mark_epoch(&self) -> u8 {
    ((self.meta.load(Ordering::Acquire) & META_MARK_MASK) >> META_MARK_SHIFT) as u8
  }

  #[inline]
  pub(crate) fn is_marked(&self, current_epoch: u8) -> bool {
    debug_assert!(current_epoch <= 1);
    self.mark_epoch() == current_epoch
  }

  #[inline]
  pub(crate) fn set_mark_epoch(&mut self, epoch: u8) {
    debug_assert!(epoch <= 1);
    if self.is_forwarded() {
      // Forwarded objects store relocation pointers in `meta` (used during minor
      // GC and optional major compaction). Mark bits are not meaningful on
      // forwarded from-space objects.
      return;
    }
    let mut meta = self.meta.load(Ordering::Relaxed);
    meta = (meta & !META_MARK_MASK) | ((epoch as usize) << META_MARK_SHIFT);
    self.meta.store(meta, Ordering::Release);
  }

  /// Returns a pointer to the per-object card table bitset, or null if the
  /// object has no card table.
  ///
  /// The pointer is stored in the high bits of `meta` (with the low flag bits
  /// reserved for GC metadata). As a result, card table allocations must be
  /// aligned such that the low [`META_FLAGS_MASK`] bits are zero.
  #[inline]
  #[doc(hidden)]
  pub fn card_table_ptr(&self) -> *mut AtomicU64 {
    if self.is_forwarded() {
      return core::ptr::null_mut();
    }
    let meta = self.meta.load(Ordering::Acquire);
    (meta & !META_FLAGS_MASK) as *mut AtomicU64
  }

  /// Install (or clear) the per-object card table pointer.
  ///
  /// # Safety
  /// The caller must ensure `ptr` points to a valid allocation containing at
  /// least `ceil(obj_size / CARD_SIZE).div_ceil(64)` [`AtomicU64`] words.
  #[inline]
  #[doc(hidden)]
  pub unsafe fn set_card_table_ptr(&mut self, ptr: *mut AtomicU64) {
    debug_assert!(
      (ptr as usize & META_FLAGS_MASK) == 0,
      "card table pointer must be aligned so low meta flag bits are free"
    );
    debug_assert!(
      !self.is_forwarded(),
      "forwarded headers must not carry card table pointers"
    );
    let flags = self.meta.load(Ordering::Relaxed) & META_FLAGS_MASK;
    self.meta.store((ptr as usize) | flags, Ordering::Release);
  }
}

/// Shape/type metadata required for precise tracing.
///
/// The offsets in `ptr_offsets` are byte offsets from the start of the object
/// (i.e. the address of [`ObjHeader`]) to each `*mut u8` pointer slot inside the
/// object.
///
/// IMPORTANT: `ptr_offsets` must list only **GC-managed** pointer fields (object references that
/// must be traced/updated by the collector). Pointers to non-GC memory (e.g. `ArrayBuffer` backing
/// stores allocated with malloc/mmap, kernel iovec buffers, etc.) must never appear here.
#[repr(C)]
pub struct TypeDescriptor {
  /// Total object size in bytes (including the [`ObjHeader`]).
  pub size: usize,
  /// Required alignment (in bytes) of the object base pointer.
  pub align: usize,
  ptr_offsets: *const u32,
  ptr_offsets_len: u32,
}

// `TypeDescriptor` is immutable runtime metadata. As long as the descriptor is
// constructed from stable, read-only data (the intended use-case), it is safe
// to share between threads.
unsafe impl Send for TypeDescriptor {}
unsafe impl Sync for TypeDescriptor {}

impl TypeDescriptor {
  pub const fn new(size: usize, ptr_offsets: &'static [u32]) -> Self {
    Self {
      size,
      align: OBJ_ALIGN,
      ptr_offsets: ptr_offsets.as_ptr(),
      ptr_offsets_len: ptr_offsets.len() as u32,
    }
  }

  pub const fn new_aligned(size: usize, align: usize, ptr_offsets: &'static [u32]) -> Self {
    Self {
      size,
      align: if align > OBJ_ALIGN { align } else { OBJ_ALIGN },
      ptr_offsets: ptr_offsets.as_ptr(),
      ptr_offsets_len: ptr_offsets.len() as u32,
    }
  }

  /// Construct a [`TypeDescriptor`] from raw pointer-offset metadata.
  ///
  /// # Safety
  /// - If `ptr_offsets_len != 0`, `ptr_offsets` must be a valid pointer to an array of
  ///   `ptr_offsets_len` `u32` elements.
  /// - The pointed-to array must remain valid and immutable for as long as this descriptor is used
  ///   (typically for the duration of the process).
  pub unsafe fn from_raw_parts(
    size: usize,
    align: usize,
    ptr_offsets: *const u32,
    ptr_offsets_len: u32,
  ) -> Self {
    Self {
      size,
      align: if align > OBJ_ALIGN { align } else { OBJ_ALIGN },
      ptr_offsets,
      ptr_offsets_len,
    }
  }

  #[inline]
  pub fn ptr_offsets(&self) -> &[u32] {
    if self.ptr_offsets_len == 0 {
      return &[];
    }
    debug_assert!(!self.ptr_offsets.is_null());
    // SAFETY: `ptr_offsets_len != 0` implies `ptr_offsets` is a valid pointer to an immutable
    // `u32` array per `TypeDescriptor::from_raw_parts`' safety contract.
    unsafe { slice::from_raw_parts(self.ptr_offsets, self.ptr_offsets_len as usize) }
  }
}

/// Common visitor interface used by both evacuation (minor GC) and marking
/// (major GC).
pub trait Tracer {
  /// Visit a slot that contains a GC reference.
  ///
  /// Implementations may update the slot in place (e.g. nursery evacuation).
  fn visit_slot(&mut self, slot: *mut *mut u8);

  /// Visit an object by scanning its pointer fields.
  fn visit_obj(&mut self, obj: *mut u8) {
    unsafe {
      for_each_ptr_slot(obj, |slot| self.visit_slot(slot));
    }
  }
}

/// Iterate over all pointer slots in `obj` as described by its
/// [`TypeDescriptor`].
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
/// - The object must be fully initialized, at least for all pointer slots.
pub(crate) unsafe fn for_each_ptr_slot(mut obj: *mut u8, mut f: impl FnMut(*mut *mut u8)) {
  debug_assert!(!obj.is_null());

  // Handle forwarding transparently: tracing should always operate on the
  // actual object body.
  let header = &*(obj as *const ObjHeader);
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = &*(obj as *const ObjHeader);
  let desc = header.type_desc();

  for &offset in desc.ptr_offsets() {
    let offset = offset as usize;
    debug_assert!(offset % mem::align_of::<*mut u8>() == 0);
    debug_assert!(offset + mem::size_of::<*mut u8>() <= desc.size);
    let slot = obj.add(offset) as *mut *mut u8;
    f(slot);
  }

  // Arrays have a dynamic pointer tail; special-case their element slots based on the header.
  if header.type_desc == &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
    array::for_each_ptr_elem_slot(obj, |slot| f(slot));
  }
}

/// Return the total size of the object at `obj` in bytes.
///
/// This is normally `obj.header.type_desc.size`, but some object kinds (notably arrays) have a
/// dynamic size derived from header fields.
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
pub(crate) unsafe fn obj_size(mut obj: *mut u8) -> usize {
  debug_assert!(!obj.is_null());

  // Follow forwarding pointers (nursery evacuation).
  let header = unsafe { &*(obj as *const ObjHeader) };
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = unsafe { &*(obj as *const ObjHeader) };
  if header.type_desc == &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
    return array::array_total_size_from_obj(obj);
  }
  unsafe { header.type_desc() }.size
}

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
mod verify;

/// Query the type-descriptor registry used by debug/verification tooling.
///
/// When the `conservative_roots` feature is enabled, conservative stack scanning
/// uses this to filter candidate pointers down to likely object headers.
#[cfg(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots"))]
pub(crate) fn is_known_type_descriptor(desc: *const TypeDescriptor) -> bool {
  verify::is_known_type_descriptor(desc)
}

#[cfg(not(any(debug_assertions, feature = "gc_debug", feature = "conservative_roots")))]
pub(crate) fn is_known_type_descriptor(_desc: *const TypeDescriptor) -> bool {
  false
}

#[cfg(test)]
mod tests;
