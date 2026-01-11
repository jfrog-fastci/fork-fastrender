use core::ptr;

use crate::abi::InternedId;
use crate::abi::StringRef;
use crate::alloc;
use crate::interner;
use crate::trap;

fn bytes_from_raw<'a>(ptr: *const u8, len: usize, context: &'static str) -> &'a [u8] {
  if len == 0 {
    return &[];
  }
  if ptr.is_null() {
    trap::rt_trap_invalid_arg(context);
  }

  // Safety: The caller promises `ptr..ptr+len` is a readable byte range.
  unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// Concatenate two UTF-8 byte strings into a new allocation.
///
/// Milestone-1 runtime: the returned bytes are heap-allocated and never freed
/// (GC not implemented yet).
#[no_mangle]
pub extern "C" fn rt_string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> StringRef {
  let a_bytes = bytes_from_raw(
    a,
    a_len,
    "rt_string_concat: `a` was null but `a_len` was non-zero",
  );
  let b_bytes = bytes_from_raw(
    b,
    b_len,
    "rt_string_concat: `b` was null but `b_len` was non-zero",
  );

  if std::str::from_utf8(a_bytes).is_err() {
    trap::rt_trap_invalid_arg("rt_string_concat: `a` was not valid UTF-8");
  }
  if std::str::from_utf8(b_bytes).is_err() {
    trap::rt_trap_invalid_arg("rt_string_concat: `b` was not valid UTF-8");
  }

  let len = a_len
    .checked_add(b_len)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string_concat: length overflow"));

  if len == 0 {
    return StringRef::empty();
  }

  let out = alloc::alloc_bytes(len, 1, "rt_string_concat");

  // Safety: `out` points to a unique allocation of `len` bytes.
  unsafe {
    ptr::copy_nonoverlapping(a_bytes.as_ptr(), out, a_len);
    ptr::copy_nonoverlapping(b_bytes.as_ptr(), out.add(a_len), b_len);
  }

  StringRef {
    ptr: out,
    len,
  }
}

/// Intern a UTF-8 byte string and return a stable ID.
#[no_mangle]
pub extern "C" fn rt_string_intern(s: *const u8, len: usize) -> InternedId {
  let bytes = bytes_from_raw(
    s,
    len,
    "rt_string_intern: `s` was null but `len` was non-zero",
  );

  if std::str::from_utf8(bytes).is_err() {
    trap::rt_trap_invalid_arg("rt_string_intern: input was not valid UTF-8");
  }

  interner::intern(bytes)
}
