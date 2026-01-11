use std::mem;
use std::slice;

pub mod heap;
pub mod roots;

mod evacuate;
mod mark;

pub use heap::GcHeap;
pub use roots::RememberedSet;
pub use roots::RootSet;
pub use roots::RootStack;

/// Object header that prefixes every GC-managed allocation.
///
/// # Layout
/// The object pointer is a raw `*mut u8` that points at the start of this header.
/// The header is followed by the object's payload as described by its
/// [`TypeDescriptor`].
#[repr(C)]
pub struct ObjHeader {
  type_desc: *const TypeDescriptor,
  meta: usize,
}

pub const OBJ_HEADER_SIZE: usize = mem::size_of::<ObjHeader>();

const META_FORWARDED: usize = 1;
const META_MARK_SHIFT: usize = 1;
const META_MARK_MASK: usize = 1 << META_MARK_SHIFT;

impl ObjHeader {
  #[inline]
  pub(crate) unsafe fn type_desc(&self) -> &TypeDescriptor {
    debug_assert!(!self.type_desc.is_null());
    &*self.type_desc
  }

  #[inline]
  pub(crate) fn is_forwarded(&self) -> bool {
    (self.meta & META_FORWARDED) != 0
  }

  #[inline]
  pub(crate) fn forwarding_ptr(&self) -> *mut u8 {
    debug_assert!(self.is_forwarded());
    (self.meta & !META_FORWARDED) as *mut u8
  }

  #[inline]
  pub(crate) fn set_forwarding_ptr(&mut self, new_location: *mut u8) {
    debug_assert!((new_location as usize & META_FORWARDED) == 0);
    self.meta = (new_location as usize) | META_FORWARDED;
  }

  #[inline]
  pub(crate) fn mark_epoch(&self) -> u8 {
    ((self.meta & META_MARK_MASK) >> META_MARK_SHIFT) as u8
  }

  #[inline]
  pub(crate) fn is_marked(&self, current_epoch: u8) -> bool {
    debug_assert!(current_epoch <= 1);
    self.mark_epoch() == current_epoch
  }

  #[inline]
  pub(crate) fn set_mark_epoch(&mut self, epoch: u8) {
    debug_assert!(epoch <= 1);
    // Preserve the forwarding bit. (Forwarded objects are only expected in the
    // nursery during minor GC, but keeping the masking logic here makes the
    // semantics explicit.)
    self.meta = (self.meta & META_FORWARDED) | ((epoch as usize) << META_MARK_SHIFT);
  }
}

/// Shape/type metadata required for precise tracing.
///
/// The offsets in `ptr_offsets` are byte offsets from the start of the object
/// (i.e. the address of [`ObjHeader`]) to each `*mut u8` pointer slot inside the
/// object.
#[repr(C)]
pub struct TypeDescriptor {
  /// Total object size in bytes (including the [`ObjHeader`]).
  pub size: usize,
  ptr_offsets: *const usize,
  ptr_offsets_len: usize,
}

// `TypeDescriptor` is immutable runtime metadata. As long as the descriptor is
// constructed from stable, read-only data (the intended use-case), it is safe
// to share between threads.
unsafe impl Send for TypeDescriptor {}
unsafe impl Sync for TypeDescriptor {}

impl TypeDescriptor {
  pub const fn new(size: usize, ptr_offsets: &'static [usize]) -> Self {
    Self {
      size,
      ptr_offsets: ptr_offsets.as_ptr(),
      ptr_offsets_len: ptr_offsets.len(),
    }
  }

  #[inline]
  pub fn ptr_offsets(&self) -> &[usize] {
    unsafe { slice::from_raw_parts(self.ptr_offsets, self.ptr_offsets_len) }
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

#[inline]
pub(crate) fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
}

/// Iterate over all pointer slots in `obj` as described by its
/// [`TypeDescriptor`].
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
/// - The object must be fully initialized, at least for all pointer slots.
pub(crate) unsafe fn for_each_ptr_slot(mut obj: *mut u8, mut f: impl FnMut(*mut *mut u8)) {
  debug_assert!(!obj.is_null());

  // Handle nursery forwarding transparently: tracing should always operate on
  // the actual object body.
  let header = &*(obj as *const ObjHeader);
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = &*(obj as *const ObjHeader);
  let desc = header.type_desc();

  for &offset in desc.ptr_offsets() {
    debug_assert!(offset % mem::align_of::<*mut u8>() == 0);
    debug_assert!(offset + mem::size_of::<*mut u8>() <= desc.size);
    let slot = obj.add(offset) as *mut *mut u8;
    f(slot);
  }
}
