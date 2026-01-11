use crate::abi::{RtShapeDescriptor, RtShapeId};
use crate::gc::{ObjHeader, TypeDescriptor};
use crate::ffi::abort_on_panic;
use std::mem;
use std::sync::OnceLock;

struct ShapeTable {
  rt_descs: Box<[RtShapeDescriptor]>,
  type_descs: Box<[TypeDescriptor]>,
}

impl ShapeTable {
  #[inline]
  fn len(&self) -> usize {
    self.rt_descs.len()
  }

  #[inline]
  fn idx(&self, id: RtShapeId) -> usize {
    if id.0 == 0 {
      panic!("RtShapeId(0) is reserved/invalid");
    }
    let idx = (id.0 - 1) as usize;
    if idx >= self.len() {
      panic!(
        "RtShapeId({}) out of bounds: shape table has {} shapes",
        id.0,
        self.len()
      );
    }
    idx
  }

  #[inline]
  fn rt_desc(&self, id: RtShapeId) -> &RtShapeDescriptor {
    &self.rt_descs[self.idx(id)]
  }

  #[inline]
  fn type_desc(&self, id: RtShapeId) -> &TypeDescriptor {
    &self.type_descs[self.idx(id)]
  }
}

static SHAPE_TABLE: OnceLock<ShapeTable> = OnceLock::new();

/// Register the global shape table used by [`RtShapeId`].
///
/// Intended to be called once during program initialization by compiler-emitted code.
///
/// # Safety
/// - `ptr` must point to an array of `len` [`RtShapeDescriptor`] values that is valid to read for
///   the duration of this call.
/// - The `ptr_offsets` arrays referenced by each descriptor must remain valid and immutable for the
///   duration of the process (codegen should emit them as static data).
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape_table(ptr: *const RtShapeDescriptor, len: usize) {
  abort_on_panic(|| register_shape_table(ptr, len));
}

pub(crate) unsafe fn register_shape_table(ptr: *const RtShapeDescriptor, len: usize) {
  if ptr.is_null() {
    panic!("rt_register_shape_table: null table pointer");
  }
  if len == 0 {
    panic!("rt_register_shape_table: len must be > 0");
  }
  if len > (u32::MAX as usize) {
    panic!("rt_register_shape_table: len too large for RtShapeId");
  }

  let table = std::slice::from_raw_parts(ptr, len);
  validate_shape_table(table);

  let mut rt_descs = Vec::with_capacity(len);
  let mut type_descs = Vec::with_capacity(len);
  for desc in table {
    rt_descs.push(*desc);
    type_descs.push(unsafe {
      TypeDescriptor::from_raw_parts(desc.size as usize, desc.ptr_offsets, desc.ptr_offsets_len)
    });
  }

  if SHAPE_TABLE
    .set(ShapeTable {
      rt_descs: rt_descs.into_boxed_slice(),
      type_descs: type_descs.into_boxed_slice(),
    })
    .is_err()
  {
    panic!("rt_register_shape_table: shape table already registered");
  }
}

#[inline]
pub fn shape_count() -> usize {
  SHAPE_TABLE.get().map(|t| t.len()).unwrap_or(0)
}

/// Lookup the registered [`RtShapeDescriptor`] for `id`.
///
/// Panics if the table is not registered or the id is invalid/out-of-bounds.
#[inline]
pub fn lookup_rt_descriptor(id: RtShapeId) -> &'static RtShapeDescriptor {
  SHAPE_TABLE
    .get()
    .unwrap_or_else(|| panic!("shape table not registered (call rt_register_shape_table first)"))
    .rt_desc(id)
}

/// Lookup the internal GC [`TypeDescriptor`] for `id`.
///
/// Panics if the table is not registered or the id is invalid/out-of-bounds.
#[inline]
pub fn lookup_type_descriptor(id: RtShapeId) -> &'static TypeDescriptor {
  SHAPE_TABLE
    .get()
    .unwrap_or_else(|| panic!("shape table not registered (call rt_register_shape_table first)"))
    .type_desc(id)
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_shape_count() -> usize {
  shape_count()
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_shape_descriptor(id: RtShapeId) -> *const RtShapeDescriptor {
  if let Some(t) = SHAPE_TABLE.get() {
    if id.0 != 0 && (id.0 as usize) <= t.len() {
      return std::ptr::from_ref(t.rt_desc(id));
    }
  }
  std::ptr::null()
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_validate_heap() {
  // The runtime does not yet have a global heap instance wired to the exported ABI. The most
  // helpful invariant we can validate today is that the shape table is present and internally
  // consistent (which registration already checks).
  if SHAPE_TABLE.get().is_none() {
    panic!("rt_debug_validate_heap: shape table not registered");
  }
}

fn validate_shape_table(table: &[RtShapeDescriptor]) {
  for (i, desc) in table.iter().enumerate() {
    validate_descriptor(i, desc);
  }
}

fn validate_descriptor(index: usize, desc: &RtShapeDescriptor) {
  if desc.flags != 0 {
    panic!("shape[{index}]: flags must be 0 (got {})", desc.flags);
  }
  if desc.reserved != 0 {
    panic!("shape[{index}]: reserved must be 0 (got {})", desc.reserved);
  }

  let size = desc.size as usize;
  if size < mem::size_of::<ObjHeader>() {
    panic!(
      "shape[{index}]: size {} too small for ObjHeader ({} bytes)",
      size,
      mem::size_of::<ObjHeader>()
    );
  }

  let align = desc.align as usize;
  if align == 0 || !align.is_power_of_two() {
    panic!("shape[{index}]: align must be a non-zero power of two (got {})", align);
  }
  if align < mem::align_of::<ObjHeader>() {
    panic!(
      "shape[{index}]: align {} smaller than ObjHeader alignment {}",
      align,
      mem::align_of::<ObjHeader>()
    );
  }

  let ptr_align = mem::align_of::<*mut u8>();
  let ptr_size = mem::size_of::<*mut u8>();

  let offsets = if desc.ptr_offsets_len == 0 {
    &[][..]
  } else {
    if desc.ptr_offsets.is_null() {
      panic!("shape[{index}]: ptr_offsets_len is non-zero but ptr_offsets is null");
    }
    // SAFETY: caller promises the table is readable for the duration of this call.
    unsafe { std::slice::from_raw_parts(desc.ptr_offsets, desc.ptr_offsets_len as usize) }
  };

  let mut last: Option<u32> = None;
  for &off_u32 in offsets {
    let off = off_u32 as usize;
    if off.checked_add(ptr_size).map_or(true, |end| end > size) {
      panic!("shape[{index}]: ptr offset {} out of bounds for size {}", off, size);
    }
    if off % ptr_align != 0 {
      panic!(
        "shape[{index}]: ptr offset {} is not pointer-aligned (align {})",
        off, ptr_align
      );
    }
    if let Some(prev) = last {
      if off_u32 <= prev {
        panic!(
          "shape[{index}]: ptr_offsets must be strictly increasing ({} then {})",
          prev, off_u32
        );
      }
    }
    last = Some(off_u32);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn register_table_and_trace_uses_offsets() {
    static LEAF_PTR_OFFSETS: [u32; 0] = [];
    static SINGLE_PTR_OFFSETS: [u32; 1] = [mem::size_of::<ObjHeader>() as u32];
    static WEIRD_PTR_OFFSETS: [u32; 1] = [(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>()) as u32];

    static TABLE: [RtShapeDescriptor; 3] = [
      RtShapeDescriptor {
        size: mem::size_of::<ObjHeader>() as u32,
        align: mem::align_of::<ObjHeader>() as u16,
        flags: 0,
        ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: (mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>()) as u32,
        align: mem::align_of::<ObjHeader>() as u16,
        flags: 0,
        ptr_offsets: SINGLE_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: SINGLE_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: (mem::size_of::<ObjHeader>() + 2 * mem::size_of::<*mut u8>()) as u32,
        align: mem::align_of::<ObjHeader>() as u16,
        flags: 0,
        ptr_offsets: WEIRD_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: WEIRD_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
    ];

    // Invalid registration is rejected (offset out of bounds).
    static INVALID_PTR_OFFSETS: [u32; 1] = [0xffff_ff00];
    static INVALID_TABLE: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<ObjHeader>() as u32,
      align: mem::align_of::<ObjHeader>() as u16,
      flags: 0,
      ptr_offsets: INVALID_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: INVALID_PTR_OFFSETS.len() as u32,
      reserved: 0,
    }];
    assert!(std::panic::catch_unwind(|| unsafe {
      register_shape_table(INVALID_TABLE.as_ptr(), INVALID_TABLE.len());
    })
    .is_err());

    unsafe {
      register_shape_table(TABLE.as_ptr(), TABLE.len());
    }

    // Shape ids are 1-indexed.
    let leaf = RtShapeId(1);
    let weird = RtShapeId(3);

    let weird_desc = lookup_type_descriptor(weird);
    assert_eq!(weird_desc.ptr_offsets(), WEIRD_PTR_OFFSETS);

    unsafe {
      // Allocate objects via the runtime ABI (`rt_alloc`) so we exercise the shape table wiring.
      let wrapper = crate::rt_alloc(weird_desc.size, weird);
      let should_live = crate::rt_alloc(mem::size_of::<ObjHeader>(), leaf);
      let should_die = crate::rt_alloc(mem::size_of::<ObjHeader>(), leaf);
      assert!(!wrapper.is_null());
      assert!(!should_live.is_null());
      assert!(!should_die.is_null());

      // Wrapper has two pointer-sized slots at offsets header+0 and header+ptr_size, but the
      // descriptor only lists the second one (WEIRD_PTR_OFFSETS).
      let base = wrapper as *mut u8;
      *base.add(mem::size_of::<ObjHeader>()).cast::<*mut u8>() = should_die;
      *base
        .add(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>())
        .cast::<*mut u8>() = should_live;

      let mut seen = Vec::<usize>::new();
      crate::gc::for_each_ptr_slot(base, |slot| {
        seen.push(slot as usize);
      });

      let expected_slot = base
        .add(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>())
        .cast::<*mut u8>() as usize;
      assert_eq!(seen, vec![expected_slot]);
    }
  }
}
