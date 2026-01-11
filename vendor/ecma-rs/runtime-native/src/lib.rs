//! Native runtime library for `native-js` AOT output.
//!
//! This crate provides:
//! - A stable C ABI surface that LLVM-generated code can link against.
//! - A precise, generational GC implementation for managed allocations.

pub mod abi;
pub mod gc;

mod alloc;
mod exports;
mod interner;
mod string;
mod trap;

pub use exports::*;
pub use gc::GcHeap;
pub use gc::RememberedSet;
pub use gc::RootSet;
pub use gc::RootStack;
pub use gc::TypeDescriptor;
pub use string::*;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn interning_is_deduplicated() {
    let id1 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
    let id2 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
    assert_eq!(id1, id2);
  }

  #[test]
  fn interning_distinguishes_bytes() {
    let id1 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
    let id2 = rt_string_intern(b"world".as_ptr(), b"world".len());
    assert_ne!(id1, id2);
  }

  #[test]
  fn concat_works() {
    let out = rt_string_concat(b"foo".as_ptr(), b"foo".len(), b"bar".as_ptr(), b"bar".len());
    assert_eq!(out.len, 6);
    // Safety: `rt_string_concat` returns a valid byte slice for the returned length.
    let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
    assert_eq!(bytes, b"foobar");
  }

  #[test]
  fn interned_lookup_roundtrip() {
    let id = rt_string_intern(b"zap".as_ptr(), b"zap".len());
    let out = crate::interner::lookup(id);
    // Safety: `lookup` returns a valid byte slice for the returned length.
    let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
    assert_eq!(bytes, b"zap");
  }
}
