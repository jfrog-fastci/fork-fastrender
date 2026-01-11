use crate::gc::ObjHeader;
use crate::gc::TypeDescriptor;
use crate::trap;

/// When set in the `elem_size` argument passed to [`rt_alloc_array`](crate::rt_alloc_array), the
/// array payload is treated as a contiguous sequence of GC pointers.
///
/// The raw element size is `elem_size & !RT_ARRAY_ELEM_PTR_FLAG` and must equal
/// `size_of::<*mut u8>()`.
pub const RT_ARRAY_ELEM_PTR_FLAG: usize = 1usize << (usize::BITS - 1);

/// Header flag: the array payload is a `len`-long sequence of `*mut u8` GC pointers.
pub const RT_ARRAY_FLAG_PTR_ELEMS: u32 = 1 << 0;

/// `TypeDescriptor` used for all runtime-native array objects.
///
/// Arrays have a dynamic length, so the GC does not rely on `TypeDescriptor.ptr_offsets()` to find
/// element slots. Instead, the GC special-cases this descriptor and uses the [`RtArrayHeader`]
/// fields to trace pointer elements and compute the total object size.
pub static RT_ARRAY_TYPE_DESC: TypeDescriptor = TypeDescriptor::new(RT_ARRAY_DATA_OFFSET, &[]);

/// The GC-managed array header.
///
/// Layout is `#[repr(C)]` and intended to be FFI-stable for codegen.
///
/// The object base pointer is a pointer to this header. The element payload starts immediately
/// after this header (at [`RT_ARRAY_DATA_OFFSET`]) and has `len * elem_size` bytes.
///
/// GC tracing:
/// - If `elem_flags & RT_ARRAY_FLAG_PTR_ELEMS != 0` and `elem_size == size_of::<*mut u8>()`,
///   then the payload is traced as `len` pointers.
/// - Otherwise the payload is treated as raw bytes (no interior pointers).
///
/// Limitation: arrays of structs with interior pointers are currently not supported; such arrays
/// must be represented as arrays of pointers (to separately-allocated objects) until the runtime
/// gains shape/bitmap-driven interior tracing.
#[repr(C)]
pub struct RtArrayHeader {
  /// Common GC header (`ObjHeader`) that prefixes all GC-managed allocations.
  pub header: ObjHeader,
  pub len: usize,
  pub elem_size: u32,
  pub elem_flags: u32,
}

/// Byte offset from the array base pointer (header) to the start of the element payload.
pub const RT_ARRAY_DATA_OFFSET: usize = core::mem::size_of::<RtArrayHeader>();

/// Decoded element metadata from the `elem_size` argument passed to `rt_alloc_array`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtArrayElemSpec {
  pub elem_size: usize,
  pub elem_flags: u32,
}

/// Decode element flags from the encoded `elem_size` value passed to `rt_alloc_array`.
///
/// `rt_alloc_array` uses the high bit of `elem_size` as a flag bitmask so the runtime can
/// distinguish:
/// - "pointer-sized raw bytes" (e.g. `u64`, `f64`) from
/// - "GC pointer elements" (payload is traced and updated by GC).
pub fn decode_rt_array_elem_size(elem_size: usize) -> Option<RtArrayElemSpec> {
  let ptr_elems = (elem_size & RT_ARRAY_ELEM_PTR_FLAG) != 0;
  let raw_elem_size = elem_size & !RT_ARRAY_ELEM_PTR_FLAG;
  if raw_elem_size == 0 {
    return None;
  }
  if raw_elem_size > u32::MAX as usize {
    return None;
  }
  if ptr_elems && raw_elem_size != core::mem::size_of::<*mut u8>() {
    return None;
  }

  Some(RtArrayElemSpec {
    elem_size: raw_elem_size,
    elem_flags: if ptr_elems { RT_ARRAY_FLAG_PTR_ELEMS } else { 0 },
  })
}

#[inline]
pub(crate) fn checked_payload_bytes(len: usize, elem_size: usize) -> Option<usize> {
  len.checked_mul(elem_size)
}

#[inline]
pub(crate) fn checked_total_bytes(len: usize, elem_size: usize) -> Option<usize> {
  RT_ARRAY_DATA_OFFSET.checked_add(checked_payload_bytes(len, elem_size)?)
}

/// Compute the total allocation size for an array object from its header.
///
/// # Safety
/// `array` must be a valid pointer to the start of an array object (the `ObjHeader` / `RtArrayHeader` base).
pub(crate) unsafe fn array_total_size_from_obj(array: *mut u8) -> usize {
  debug_assert!(!array.is_null());
  let header = unsafe { &*(array as *const RtArrayHeader) };
  let elem_size = header.elem_size as usize;
  checked_total_bytes(header.len, elem_size).unwrap_or_else(|| trap::rt_trap_invalid_arg("array size overflow"))
}

/// Iterate over all pointer element slots in `array` (if any).
///
/// # Safety
/// `array` must point to a valid array object base pointer.
pub(crate) unsafe fn for_each_ptr_elem_slot(array: *mut u8, mut f: impl FnMut(*mut *mut u8)) {
  debug_assert!(!array.is_null());
  let header = unsafe { &*(array as *const RtArrayHeader) };
  if (header.elem_flags & RT_ARRAY_FLAG_PTR_ELEMS) == 0 {
    return;
  }
  if header.elem_size as usize != core::mem::size_of::<*mut u8>() {
    trap::rt_trap_invalid_arg("pointer array elem_size must equal pointer size");
  }

  let base = unsafe { array.add(RT_ARRAY_DATA_OFFSET) as *mut *mut u8 };
  for i in 0..header.len {
    // SAFETY: payload is `len * elem_size` bytes and we validated `elem_size == ptr_size`.
    let slot = unsafe { base.add(i) };
    f(slot);
  }
}

/// Returns a pointer to the start of the element payload for a given array base pointer.
///
/// # Safety
/// `array` must be a pointer returned by `rt_alloc_array` (or null).
#[inline]
pub unsafe fn array_data_ptr(array: *mut u8) -> *mut u8 {
  if array.is_null() {
    return core::ptr::null_mut();
  };
  // SAFETY: caller promises `array` points to a valid array allocation base.
  unsafe { array.add(RT_ARRAY_DATA_OFFSET) }
}
