//! Native runtime library for `native-js` AOT output.
//!
//! This crate provides:
//! - A stable C ABI surface that LLVM-generated code can link against.
//! - A precise, generational GC implementation for managed allocations.
//!
//! See:
//! - `docs/write_barrier.md` for the generational GC write barrier contract.
//! - `include/runtime_native.h` for the minimal stable C ABI surface.

pub mod abi;
pub mod gc;
pub mod threading;

mod alloc;
mod async_rt;
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

/// Request a stop-the-world GC safepoint.
///
/// Internal runtime hook; not a stable public API.
#[doc(hidden)]
pub fn rt_gc_request_stop_the_world() -> u64 {
  threading::safepoint::rt_gc_request_stop_the_world()
}

/// Block until all registered threads are at a GC safepoint (or parked).
///
/// Internal runtime hook; not a stable public API.
#[doc(hidden)]
pub fn rt_gc_wait_for_world_stopped() {
  threading::safepoint::rt_gc_wait_for_world_stopped()
}

/// Like [`rt_gc_wait_for_world_stopped`], but with a timeout.
///
/// Returns `true` if the world stopped in time, `false` otherwise.
#[doc(hidden)]
pub fn rt_gc_wait_for_world_stopped_timeout(timeout: std::time::Duration) -> bool {
  threading::safepoint::rt_gc_wait_for_world_stopped_timeout(timeout)
}

/// Resume all threads after a stop-the-world GC safepoint.
///
/// Internal runtime hook; not a stable public API.
#[doc(hidden)]
pub fn rt_gc_resume_world() -> u64 {
  threading::safepoint::rt_gc_resume_world()
}

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

  #[test]
  fn c_header_matches_exported_gc_entrypoints() {
    const HEADER: &str = include_str!("../include/runtime_native.h");

    const GC_SAFEPOINT: &str = "void rt_gc_safepoint(void);";
    const WRITE_BARRIER: &str = "void rt_write_barrier(uint8_t* obj, uint8_t* slot);";
    const GC_COLLECT: &str = "void rt_gc_collect(void);";

    for decl in [GC_SAFEPOINT, WRITE_BARRIER, GC_COLLECT] {
      assert!(
        HEADER.contains(decl),
        "`runtime_native.h` is missing expected declaration: {decl}"
      );
    }

    // Ensure the Rust exports match the declared ABI shape.
    let _safepoint: extern "C" fn() = rt_gc_safepoint;
    let _write_barrier: extern "C" fn(*mut u8, *mut u8) = rt_write_barrier;
    let _collect: extern "C" fn() = rt_gc_collect;
    let _ = (_safepoint, _write_barrier, _collect);
  }
}
