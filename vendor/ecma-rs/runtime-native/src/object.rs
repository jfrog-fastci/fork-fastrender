use crate::metadata::TypeDescriptor;

/// Nursery-only: the object has been evacuated; [`Header::meta`] stores the new
/// object pointer.
pub const FLAG_FORWARDED: usize = 1 << 0;
/// Old-generation-only: the object is in the remembered set.
pub const FLAG_REMEMBERED: usize = 1 << 1;
/// Major GC mark bit (intended to be epoch-colored later).
pub const FLAG_MARK: usize = 1 << 2;
/// Object cannot move (e.g. FFI references, large object space).
pub const FLAG_PINNED: usize = 1 << 3;

const FLAG_MASK: usize = FLAG_FORWARDED | FLAG_REMEMBERED | FLAG_MARK | FLAG_PINNED;

/// A GC object header.
///
/// Layout (two machine words, 16 bytes on 64-bit targets):
/// - Word 0: `type_ptr_and_flags` — pointer to a [`TypeDescriptor`] with low bits
///   used as flags.
/// - Word 1: `meta` — length for variable-size objects, or forwarding pointer
///   during evacuation.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Header {
  pub type_ptr_and_flags: usize,
  pub meta: usize,
}

impl Header {
  #[inline]
  pub fn new(desc: *const TypeDescriptor) -> Self {
    debug_assert_eq!((desc as usize) & FLAG_MASK, 0, "TypeDescriptor must be >=16-byte aligned");
    Self {
      type_ptr_and_flags: desc as usize,
      meta: 0,
    }
  }

  #[inline]
  pub fn type_desc(&self) -> *const TypeDescriptor {
    (self.type_ptr_and_flags & !FLAG_MASK) as *const TypeDescriptor
  }

  #[inline]
  pub fn set_type_desc(&mut self, desc: *const TypeDescriptor) {
    debug_assert_eq!((desc as usize) & FLAG_MASK, 0, "TypeDescriptor must be >=16-byte aligned");
    self.type_ptr_and_flags = (desc as usize) | self.flags();
  }

  #[inline]
  pub fn flags(&self) -> usize {
    self.type_ptr_and_flags & FLAG_MASK
  }

  #[inline]
  pub fn has_flag(&self, flag: usize) -> bool {
    (self.type_ptr_and_flags & flag) != 0
  }

  #[inline]
  pub fn set_flag(&mut self, flag: usize) {
    self.type_ptr_and_flags |= flag;
  }

  #[inline]
  pub fn clear_flag(&mut self, flag: usize) {
    self.type_ptr_and_flags &= !flag;
  }
}

/// Convert a GC-visible object pointer to its header pointer.
///
/// # Invariant
/// `obj` points to the start of the object header (i.e. `obj` is the object base
/// pointer). This matches the `runtime-native` ABI contract for GC object
/// pointers.
#[inline]
pub unsafe fn header_from_obj(obj: *mut u8) -> *mut Header {
  obj.cast::<Header>()
}

/// Convert a header pointer to the GC-visible object pointer (object base
/// pointer).
///
/// # Invariant
/// `h` points to the start of an allocation that contains a [`Header`] followed
/// immediately by the object payload.
#[inline]
pub unsafe fn obj_from_header(h: *mut Header) -> *mut u8 {
  h.cast::<u8>()
}

#[inline]
pub unsafe fn type_desc(obj: *mut u8) -> *const TypeDescriptor {
  (*header_from_obj(obj)).type_desc()
}

#[inline]
pub unsafe fn is_forwarded(obj: *mut u8) -> bool {
  (*header_from_obj(obj)).has_flag(FLAG_FORWARDED)
}

/// Mark `obj` as forwarded and record the new object pointer in the header.
#[inline]
pub unsafe fn forward_to(obj: *mut u8, new_obj: *mut u8) {
  let header = &mut *header_from_obj(obj);
  header.set_flag(FLAG_FORWARDED);
  header.meta = new_obj as usize;
}

#[inline]
pub unsafe fn forwarding_ptr(obj: *mut u8) -> *mut u8 {
  let header = &*header_from_obj(obj);
  debug_assert!(header.has_flag(FLAG_FORWARDED));
  header.meta as *mut u8
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::metadata::{TypeDescriptor, TypeKind};

  static PTR_OFFSETS: [u32; 0] = [];
  static DUMMY_DESC: TypeDescriptor = TypeDescriptor {
    kind: TypeKind::Fixed,
    size: 0,
    ptr_offsets: &PTR_OFFSETS,
  };

  #[test]
  fn header_is_16_bytes_on_64bit() {
    if cfg!(target_pointer_width = "64") {
      assert_eq!(core::mem::size_of::<Header>(), 16);
    }
  }

  #[test]
  fn header_obj_roundtrip() {
    use std::alloc::{alloc, dealloc, Layout};

    unsafe {
      let layout =
        Layout::from_size_align(core::mem::size_of::<Header>() + 32, core::mem::align_of::<Header>())
          .unwrap();
      let base = alloc(layout);
      assert!(!base.is_null());

      let h = base.cast::<Header>();
      let obj = obj_from_header(h);
      // The runtime ABI uses **base-pointer** object references: the GC-visible
      // object pointer is the address of the header itself (not the payload
      // after the header).
      assert_eq!(obj, h.cast::<u8>());
      assert_eq!(header_from_obj(obj), h);
      assert_eq!(obj_from_header(header_from_obj(obj)), obj);

      dealloc(base, layout);
    }
  }

  #[test]
  fn flags_set_clear() {
    let mut h = Header::new(&DUMMY_DESC);
    assert_eq!(h.flags(), 0);

    h.set_flag(FLAG_MARK);
    assert!(h.has_flag(FLAG_MARK));

    h.set_flag(FLAG_PINNED);
    assert!(h.has_flag(FLAG_PINNED));

    h.clear_flag(FLAG_MARK);
    assert!(!h.has_flag(FLAG_MARK));
    assert!(h.has_flag(FLAG_PINNED));
  }

  #[test]
  fn forwarding_helpers() {
    use std::alloc::{alloc, dealloc, Layout};

    unsafe {
      let layout =
        Layout::from_size_align(core::mem::size_of::<Header>() + 8, core::mem::align_of::<Header>())
          .unwrap();

      let base_a = alloc(layout);
      let base_b = alloc(layout);
      assert!(!base_a.is_null());
      assert!(!base_b.is_null());

      let h_a = base_a.cast::<Header>();
      let h_b = base_b.cast::<Header>();
      h_a.write(Header::new(&DUMMY_DESC));
      h_b.write(Header::new(&DUMMY_DESC));

      let obj_a = obj_from_header(h_a);
      let obj_b = obj_from_header(h_b);

      assert!(!is_forwarded(obj_a));
      forward_to(obj_a, obj_b);
      assert!(is_forwarded(obj_a));
      assert_eq!(forwarding_ptr(obj_a), obj_b);

      dealloc(base_a, layout);
      dealloc(base_b, layout);
    }
  }
}
