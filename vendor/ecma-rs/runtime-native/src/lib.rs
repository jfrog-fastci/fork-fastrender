//! Native runtime library for `native-js` AOT output.
//!
//! This crate provides:
//! - A stable C ABI surface that LLVM-generated code can link against.
//! - A precise, generational GC implementation for managed allocations.
//!
//! See:
//! - `docs/write_barrier.md` for the generational GC write barrier contract.
//! - `include/runtime_native.h` for the stable C ABI surface.

pub mod abi;
pub mod gc;
pub mod immix;
pub mod nursery;
pub mod threading;
pub mod async_rt;
pub mod stackmaps;
pub mod statepoints;

mod alloc;
mod exports;
mod interner;
mod parallel;
mod platform;
mod string;
mod trap;

pub use exports::*;
pub use gc::GcHeap;
pub use gc::RememberedSet;
pub use gc::RootSet;
pub use gc::RootStack;
pub use gc::TypeDescriptor;
pub use string::*;

use std::sync::OnceLock;

struct Runtime {
  parallel: parallel::ParallelRuntime,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn rt_ensure_init() -> &'static Runtime {
  RUNTIME.get_or_init(|| Runtime {
    parallel: parallel::ParallelRuntime::new(),
  })
}

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
  fn c_header_matches_exported_entrypoints() {
    const HEADER: &str = include_str!("../include/runtime_native.h");

    // Keep these strings in sync with `include/runtime_native.h` to ensure we
    // don't forget to update the header when changing the exported ABI.
    const DECLS: &[&str] = &[
      "uint8_t* rt_alloc(size_t size, ShapeId shape);",
      "uint8_t* rt_alloc_array(size_t len, size_t elem_size);",
      "void rt_gc_safepoint(void);",
      "void rt_write_barrier(uint8_t* obj, uint8_t* slot);",
      "void rt_gc_collect(void);",
      "void rt_gc_set_young_range(uint8_t* start, uint8_t* end);",
      "void rt_gc_get_young_range(uint8_t** out_start, uint8_t** out_end);",
      "StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);",
      "InternedId rt_string_intern(const uint8_t* s, size_t len);",
      "TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);",
      "void rt_parallel_join(const TaskId* tasks, size_t count);",
      "PromiseRef rt_async_spawn(RtCoroutineHeader* coro);",
      "bool rt_async_poll(void);",
      "PromiseRef rt_promise_new(void);",
      "void rt_promise_resolve(PromiseRef p, ValueRef value);",
      "void rt_promise_reject(PromiseRef p, ValueRef err);",
      "void rt_promise_then(PromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);",
      "void rt_coro_await(RtCoroutineHeader* coro, PromiseRef awaited, uint32_t next_state);",
    ];

    for decl in DECLS {
      assert!(
        HEADER.contains(decl),
        "`runtime_native.h` is missing expected declaration: {decl}"
      );
    }

    // Ensure the Rust exports match the declared ABI shape.
    let _alloc: extern "C" fn(usize, abi::ShapeId) -> *mut u8 = rt_alloc;
    let _alloc_array: extern "C" fn(usize, usize) -> *mut u8 = rt_alloc_array;
    let _safepoint: extern "C" fn() = rt_gc_safepoint;
    let _write_barrier: unsafe extern "C" fn(*mut u8, *mut u8) = rt_write_barrier;
    let _collect: extern "C" fn() = rt_gc_collect;
    let _set_young_range: extern "C" fn(*mut u8, *mut u8) = rt_gc_set_young_range;
    let _get_young_range: unsafe extern "C" fn(*mut *mut u8, *mut *mut u8) = rt_gc_get_young_range;
    let _concat: extern "C" fn(*const u8, usize, *const u8, usize) -> abi::StringRef = rt_string_concat;
    let _intern: extern "C" fn(*const u8, usize) -> abi::InternedId = rt_string_intern;
    let _spawn: extern "C" fn(extern "C" fn(*mut u8), *mut u8) -> abi::TaskId = rt_parallel_spawn;
    let _join: extern "C" fn(*const abi::TaskId, usize) = rt_parallel_join;
    let _async_spawn: extern "C" fn(*mut abi::RtCoroutineHeader) -> abi::PromiseRef = rt_async_spawn;
    let _async_poll: extern "C" fn() -> bool = rt_async_poll;
    let _promise_new: extern "C" fn() -> abi::PromiseRef = rt_promise_new;
    let _promise_resolve: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_resolve;
    let _promise_reject: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_reject;
    let _promise_then: extern "C" fn(abi::PromiseRef, extern "C" fn(*mut u8), *mut u8) = rt_promise_then;
    let _coro_await: extern "C" fn(*mut abi::RtCoroutineHeader, abi::PromiseRef, u32) = rt_coro_await;
    let _ = (
      _alloc,
      _alloc_array,
      _safepoint,
      _write_barrier,
      _collect,
      _set_young_range,
      _get_young_range,
      _concat,
      _intern,
      _spawn,
      _join,
      _async_spawn,
      _async_poll,
      _promise_new,
      _promise_resolve,
      _promise_reject,
      _promise_then,
      _coro_await,
    );
  }
}
