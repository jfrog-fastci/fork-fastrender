//! GC metadata and precise tracing.
//!
//! ## `TypeDescriptor`
//! This runtime uses a simple, static description of an object's pointer layout
//! for precise tracing.
//!
//! Representation: **offset list** (plan option A).
//!
//! - Pointer slots are described as byte offsets from the start of the **object
//!   payload** (i.e. the pointer returned from allocation, immediately after the
//!   16-byte [`crate::object::Header`]).
//! - Fixed-size objects have a non-zero `size` (payload size in bytes).
//! - Variable-size objects have `size = 0` and use [`crate::object::Header::meta`]
//!   as a length:
//!   - [`TypeKind::PtrArray`]: `meta` is element count, elements are `*mut u8`
//!   - [`TypeKind::ByteArray`]: `meta` is byte length, no pointers
//!
//! The GC traces an object by calling [`trace_object`], which iterates each
//! mutable pointer slot and yields it to a visitor callback.

use crate::object::{header_from_obj, type_desc};

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TypeKind {
  /// Fixed-size object; trace pointer fields via `ptr_offsets`.
  Fixed = 0,
  /// Variable-size array of `*mut u8`; length is stored in header `meta`.
  PtrArray = 1,
  /// Variable-size byte array (e.g. string bytes); length is stored in header `meta`.
  ByteArray = 2,
}

/// Describes the pointer layout (and size) of a GC-managed object.
///
/// This struct is embedded by pointer into object headers with low bits reserved
/// for GC flags; it is therefore required to be aligned to at least 16 bytes.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct TypeDescriptor {
  pub kind: TypeKind,
  /// Payload size in bytes for fixed-size objects; `0` for variable-sized kinds.
  pub size: usize,
  /// Pointer slot offsets from the start of the object payload.
  pub ptr_offsets: &'static [u32],
}

impl TypeDescriptor {
  pub const fn fixed(size: usize, ptr_offsets: &'static [u32]) -> Self {
    Self {
      kind: TypeKind::Fixed,
      size,
      ptr_offsets,
    }
  }

  pub const fn ptr_array() -> Self {
    Self {
      kind: TypeKind::PtrArray,
      size: 0,
      ptr_offsets: &[],
    }
  }

  pub const fn byte_array() -> Self {
    Self {
      kind: TypeKind::ByteArray,
      size: 0,
      ptr_offsets: &[],
    }
  }
}

/// Trace an object precisely.
///
/// # Safety
/// - `obj` must be a valid pointer to an object payload allocated by this runtime.
/// - The object header must contain a valid `TypeDescriptor` pointer.
/// - All offsets (for fixed objects) must be in-bounds and point to properly
///   aligned `*mut u8` slots.
///
/// The visitor is passed a pointer to each slot, allowing the GC to update it
/// in-place.
#[inline]
pub unsafe fn trace_object(obj: *mut u8, mut visit: impl FnMut(*mut *mut u8)) {
  let desc = &*type_desc(obj);
  match desc.kind {
    TypeKind::Fixed => {
      for &off in desc.ptr_offsets {
        let slot = obj.add(off as usize).cast::<*mut u8>();
        visit(slot);
      }
    }
    TypeKind::PtrArray => {
      let len = (*header_from_obj(obj)).meta;
      let stride = core::mem::size_of::<*mut u8>();
      for i in 0..len {
        let slot = obj.add(i * stride).cast::<*mut u8>();
        visit(slot);
      }
    }
    TypeKind::ByteArray => {
      // No pointers.
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::object::{obj_from_header, Header};

  #[test]
  fn trace_object_visits_expected_slots() {
    use std::alloc::{alloc, dealloc, Layout};

    #[repr(C)]
    struct DummyObj {
      a: *mut u8,
      b: u64,
      c: *mut u8,
    }

    const OFFSETS: [u32; 2] = [0, 16];
    let desc = TypeDescriptor::fixed(core::mem::size_of::<DummyObj>(), &OFFSETS);

    unsafe {
      let layout = Layout::from_size_align(
        core::mem::size_of::<Header>() + core::mem::size_of::<DummyObj>(),
        core::mem::align_of::<Header>(),
      )
      .unwrap();

      let base = alloc(layout);
      assert!(!base.is_null());

      let header = base.cast::<Header>();
      header.write(Header::new(&desc));
      let obj = obj_from_header(header).cast::<DummyObj>();

      let mut x = 1u8;
      let mut y = 2u8;
      obj.write(DummyObj {
        a: (&mut x as *mut u8),
        b: 123,
        c: (&mut y as *mut u8),
      });

      let expected_a = obj.cast::<u8>().add(0).cast::<*mut u8>();
      let expected_c = obj.cast::<u8>().add(16).cast::<*mut u8>();

      let mut visited = Vec::<*mut *mut u8>::new();
      trace_object(obj.cast::<u8>(), |slot| {
        visited.push(slot);
        *slot = core::ptr::null_mut();
      });

      assert_eq!(visited, vec![expected_a, expected_c]);

      let obj_ref = &*obj;
      assert!(obj_ref.a.is_null());
      assert!(obj_ref.c.is_null());

      dealloc(base, layout);
    }
  }
}
