//! ABI/layout helpers for `runtime-native` GC-managed arrays (`rt_alloc_array`).
//!
//! `native-js` lowers TypeScript/JS array and tuple operations directly against the
//! runtime's stable array header/payload layout (as defined by `runtime-native-abi`).
//! Centralizing these constants avoids duplicating "magic offsets" in codegen.
//!
//! Note: the runtime-native ABI is currently 64-bit only.

use runtime_native_abi::{RtArrayHeader, RT_ARRAY_DATA_OFFSET, RT_ARRAY_ELEM_PTR_FLAG};

/// Byte offset of the `len: usize` field within [`RtArrayHeader`].
pub const RT_ARRAY_LEN_OFFSET: usize = core::mem::offset_of!(RtArrayHeader, len);

/// Byte offset from the array base pointer to the start of the element payload.
pub const RT_ARRAY_DATA_OFFSET_BYTES: usize = RT_ARRAY_DATA_OFFSET;

/// Flag bit for the `elem_size` argument to `rt_alloc_array` indicating the payload is
/// a contiguous sequence of GC pointers.
pub const RT_ARRAY_ELEM_PTR_FLAG_BITS: usize = RT_ARRAY_ELEM_PTR_FLAG;
