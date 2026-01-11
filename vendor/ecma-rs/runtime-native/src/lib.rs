// We use the unstable `#[thread_local]` attribute so the JIT/native codegen can
// access the current thread record via a single TLS load.
//
// `vendor/ecma-rs/.cargo/config.toml` sets `RUSTC_BOOTSTRAP=1` for the workspace
// (required by `cargo fuzz`), which also allows this crate to use the feature
// gate until `#[thread_local]` is stabilized for plain-data statics.
#![feature(thread_local)]

//! Native runtime library for `native-js` AOT output.
//!
//! This crate provides:
//! - A stable C ABI surface that LLVM-generated code can link against.
//! - A stop-the-world generational GC implementation (`gc::GcHeap`) used by tests and future ABI wiring.
//!
//! Note: the current exported allocation/collection entrypoints (`rt_alloc`, `rt_gc_collect`) are
//! still backed by `malloc` / stubs; the exported write barrier is implemented and performs
//! young-range checks + sets the per-object remembered bit (see `docs/write_barrier.md`).
//!
//! ## Pinned allocations
//!
//! Some consumers (FFI / host embeddings) require stable object addresses. The GC supports
//! **pinned objects** allocated in a non-moving space (the Large Object Space / LOS):
//!
//! - They are never moved by minor GC, major GC, or optional compaction.
//! - They are still traced and reclaimed when unreachable.
//!
//! ## ArrayBuffer backing stores
//!
//! OS syscalls (especially async I/O like io_uring) require buffers remain valid at a stable
//! address until completion. Under a moving GC this means `ArrayBuffer`/`TypedArray` data must live
//! outside the GC heap. The [`buffer`] module provides a non-moving backing store allocator along
//! with movable header structs (`ArrayBuffer`, `Uint8Array`).
//! ## GC-safe host queues (persistent roots)
//!
//! Host-owned work queues (async tasks, I/O watchers, OS event loop userdata, etc.) are **not**
//! automatically traced by the GC. Any queued work that captures GC-managed objects must keep those
//! objects alive explicitly.
//!
//! This crate provides [`gc::HandleTable`], a generational handle table intended to act like a
//! *persistent root set*:
//!
//! - Hosts store a stable [`gc::HandleId`] (convertible to/from `u64`) in their queues or OS
//!   userdata.
//! - The table stores a relocatable [`core::ptr::NonNull`] pointer.
//! - During relocation/compaction the GC updates pointers in-place via
//!   [`gc::HandleTable::update`] / [`gc::HandleTable::iter_live_mut`] under a stop-the-world (STW)
//!   pause.
//!
//! The GC-managed objects themselves remain movable; only the handle IDs and handle table slots are
//! stable.
//!
//! See:
//! - `docs/gc_handle_abi.md` for the handle-based ABI contract for `may_gc` runtime entrypoints.
//! - `docs/write_barrier.md` for the generational GC write barrier contract.
//! - `include/runtime_native.h` for the authoritative stable C ABI surface.

pub mod abi;
pub mod array;
pub mod arch;
pub mod gc_safe;
pub mod async_abi;
pub mod async_rt;
pub mod async_runtime;
pub mod promise_reactions;
pub mod reactor;
pub mod timer_wheel;
pub mod time;
pub mod gc;
pub mod gc_roots;
pub mod io;
pub mod buffer;
pub mod immix;
pub mod los;
pub mod nursery;
pub mod roots;
pub mod stackmap;
pub mod parallel;
pub mod safepoint;
pub mod sync;
pub mod threading;
pub mod runtime;
pub mod thread;
pub mod thread_registry;
pub mod stackmaps;
pub mod stackmaps_validate;
pub mod stackmaps_loader;
pub mod stackmap_loader;
pub mod statepoints;
pub mod scan;
pub mod stack_walk;
pub mod stackwalk;
pub mod stackwalk_fp;
pub mod test_util;
pub mod statepoint_verify;

// Loom model-checking harness for the promise waiter protocol.
//
// Keep it available to integration tests, but don't treat it as stable API.
#[cfg(any(test, feature = "loom"))]
#[allow(dead_code)]
pub mod loom_promise_waiters;

// Shape/type registration table used by the GC and generated code.
pub mod shape_table;

mod alloc;
#[cfg(feature = "gc_stats")]
mod gc_stats;
mod unhandled_rejection;
mod blocking_pool;
mod ffi;
mod exports;
mod parallel_integration;
mod interner;
mod native_async;
mod platform;
mod rt_trace;
mod string;
mod trap;

// Convenience re-exports of the stable runtime ABI types (single source of truth
// lives in `runtime-native-abi`).
pub use runtime_native_abi::{
  Coroutine as AbiCoroutine,
  InternedId,
  PromiseRef,
  PromiseRef as AbiPromiseRef,
  RtParallelForBodyFn,
  RtShapeDescriptor,
  RtShapeId,
  RtTaskFn,
  StringRef,
  TaskId,
};

pub use exports::*;
pub use async_abi::*;
pub use async_runtime::{rt_async_run_until_idle, rt_drain_microtasks, PromiseLayout};
pub use buffer::{
  global_backing_store_allocator, ArrayBuffer, ArrayBufferError, BackingStore, BackingStoreAllocError,
  BackingStoreAllocator, BackingStoreDetachError, BackingStorePinError, GlobalBackingStoreAllocator,
  PinnedArrayBuffer, PinnedBackingStore, PinnedUint8Array, TypedArrayError, Uint8Array,
  BACKING_STORE_MIN_ALIGN,
};
pub use gc::GcHeap;
pub use gc::{HandleId, HandleTable, OwnedGcHandle, PersistentHandle};
pub use gc::RememberedSet;
pub use gc::PersistentRoot;
pub use gc::RootHandle;
pub use gc::RootSet;
pub use gc::RootStack;
pub use gc::TypeDescriptor;
pub use async_rt::set_strict_await_yields;
pub use thread_registry::{
  rt_register_current_thread, rt_register_thread, rt_unregister_current_thread, rt_unregister_thread,
};
pub use gc_roots::{RelocPair, StackRootEnumerator};
pub use stackmaps::StackMaps;
pub use stackmaps_validate::{validate_stackmaps, ValidationError};
pub use stackwalk_fp::{
  relocate_pair, walk_gc_root_pairs_from_fp, walk_gc_root_pairs_from_safepoint_context,
  walk_gc_roots_from_fp, StatepointRootPair, WalkError,
};
pub use rt_trace::rt_debug_snapshot_counters;
pub use rt_trace::RtDebugCountersSnapshot;
pub use stack_walk::{FrameView, StackWalker};
pub use string::*;
pub use timer_wheel::{TimerKey, TimerWheel};
pub use stackmaps_loader::{load_stackmaps_from_self, stackmaps_section, try_load_via_linker_symbols};
pub use safepoint::{visit_reloc_pairs, with_world_stopped};
pub use stackmap_loader::{build_global_stackmap_index, load_all_llvm_stackmaps, StackMapIndex};
pub use runtime::{AttachError, DetachError, Runtime, StopTheWorldGuard, ThreadGuard};
pub use thread::{
  current_thread, current_thread_mut, current_thread_ptr, current_thread_state, Thread, ThreadState, RT_THREAD,
};

use std::sync::OnceLock;

struct GlobalRuntime {
  /// Lazily-initialized parallel worker pool.
  ///
  /// Creating worker threads can take noticeable time; keep this on-demand so
  /// `rt_async_poll` (and other non-parallel entrypoints) don't pay the cost on
  /// first use.
  parallel: OnceLock<parallel::ParallelRuntime>,
}

static RUNTIME: OnceLock<GlobalRuntime> = OnceLock::new();

fn rt_ensure_init() -> &'static GlobalRuntime {
  RUNTIME.get_or_init(|| GlobalRuntime {
    parallel: OnceLock::new(),
  })
}

fn rt_parallel() -> &'static parallel::ParallelRuntime {
  rt_ensure_init()
    .parallel
    .get_or_init(|| parallel::ParallelRuntime::new())
}

/// Spawn an async coroutine and return its result promise.
///
/// Generated code allocates a coroutine frame whose first field is a [`Coroutine`] header and calls
/// this function to start execution. The runtime must allocate the coroutine's result promise (using
/// metadata in [`CoroutineVTable`]) and store it into `coro.promise` before resuming.
///
/// # Safety
/// `coro` must point to a valid coroutine frame whose prefix matches [`Coroutine`].
#[no_mangle]
pub unsafe extern "C" fn rt_async_spawn(coro: CoroutineRef) -> PromiseRef {
  crate::ffi::abort_on_panic(|| crate::native_async::async_spawn(coro))
}

/// Like [`rt_async_spawn`], but enqueues the coroutine's first resume as a microtask instead of
/// running it synchronously.
///
/// This is required for Web-style microtask semantics (e.g. `queueMicrotask`).
///
/// # Safety
/// `coro` must point to a valid coroutine frame whose prefix matches [`Coroutine`].
#[no_mangle]
pub unsafe extern "C" fn rt_async_spawn_deferred(coro: CoroutineRef) -> PromiseRef {
  crate::ffi::abort_on_panic(|| crate::native_async::async_spawn_deferred(coro))
}

/// Drive the async scheduler/executor.
///
/// Returns `true` if there is still pending work after this turn (queued
/// microtasks/macrotasks, timers, or I/O watchers).
///
/// Returns `false` when the runtime is fully idle.
#[no_mangle]
pub extern "C" fn rt_async_poll() -> bool {
  // Reuse the existing JS-shaped event loop for now so timers and I/O driven by
  // `async_rt` make progress from Rust tests and generated code.
  crate::ffi::abort_on_panic(|| crate::exports::rt_async_poll_legacy())
}

/// Initialize a newly allocated promise header to the pending state.
///
/// This is part of the stable native async ABI defined in [`async_abi`]. The
/// promise's payload begins immediately after the [`PromiseHeader`] prefix.
///
/// # Safety
/// `p` must point to a valid [`PromiseHeader`] at offset 0 of a promise allocation.
#[no_mangle]
pub unsafe extern "C" fn rt_promise_init(_p: PromiseRef) {
  crate::ffi::abort_on_panic(|| crate::native_async::promise_init(_p))
}

/// Mark a promise as fulfilled.
///
/// The promise's payload must already have been written by the caller.
///
/// # Safety
/// `p` must point to a valid promise allocation.
#[no_mangle]
pub unsafe extern "C" fn rt_promise_fulfill(_p: PromiseRef) {
  crate::ffi::abort_on_panic(|| crate::native_async::promise_fulfill(_p))
}

/// Mark a promise as rejected.
///
/// # Safety
/// `p` must point to a valid promise allocation.
#[no_mangle]
pub unsafe extern "C" fn rt_promise_reject(_p: PromiseRef) {
  crate::ffi::abort_on_panic(|| crate::native_async::promise_reject(_p))
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

// Unit tests need a `.llvm_stackmaps` section to validate the linker-script
// based loader without requiring LLVM statepoints in the Rust code.
//
// When the build script can generate and link `stackmap_test.o`, we already have
// a real `.llvm_stackmaps` section and must not add a dummy one (the format is
// not concatenation-friendly). Otherwise, add a minimal, valid StackMap v3
// header so `stackmaps_section()` can find something to parse.
#[cfg(all(
  test,
  target_os = "linux",
  runtime_native_no_stackmap_test_artifact
))]
#[link_section = ".llvm_stackmaps"]
#[no_mangle]
pub static __RUNTIME_NATIVE_DUMMY_STACKMAPS: [u8; 16] = [
  // Version (v3)
  3, 0, 0, 0, // reserved + padding to u32 boundary
  // NumFunctions, NumConstants, NumRecords
  0, 0, 0, 0, //
  0, 0, 0, 0, //
  0, 0, 0, 0, //
];

#[cfg(test)]
#[no_mangle]
pub extern "C-unwind" fn rt_async_test_panic() {
  crate::ffi::abort_on_panic(|| panic!("intentional panic to verify FFI abort boundary"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::time::{Duration, Instant};

  extern "C" {
    fn rt_gc_safepoint_slow(epoch: u64);
  }

  #[test]
  fn interning_is_deduplicated() {
    crate::interner::with_test_lock(|| {
      let id1 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
      let id2 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
      assert_eq!(id1, id2);
    });
  }

  #[test]
  fn interning_distinguishes_bytes() {
    crate::interner::with_test_lock(|| {
      let id1 = rt_string_intern(b"hello".as_ptr(), b"hello".len());
      let id2 = rt_string_intern(b"world".as_ptr(), b"world".len());
      assert_ne!(id1, id2);
    });
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
  fn gc_safepoint_poll_symbol_is_exported() {
    extern "C" {
      #[link_name = "gc.safepoint_poll"]
      fn gc_safepoint_poll();
    }

    // `gc.safepoint_poll` is the symbol LLVM's `place-safepoints` inserts into
    // GC-managed functions. It cooperates with the runtime's stop-the-world
    // protocol and may publish safepoint metadata into the per-thread registry on
    // the slow path. Ensure this test thread is registered without clobbering an
    // existing registration that may be shared across tests.
    let was_registered = crate::threading::registry::current_thread_id().is_some();
    if !was_registered {
      rt_thread_init(0);
    }
    struct Deinit {
      was_registered: bool,
    }
    impl Drop for Deinit {
      fn drop(&mut self) {
        if !self.was_registered {
          rt_thread_deinit();
        }
      }
    }
    let _deinit = Deinit { was_registered };

    // Safety: the symbol is exported by this crate. When no stop-the-world GC is
    // requested, the fast path returns immediately.
    unsafe {
      gc_safepoint_poll();
    }
  }

  #[test]
  fn interned_lookup_roundtrip() {
    crate::interner::with_test_lock(|| {
      let id = rt_string_intern(b"zap".as_ptr(), b"zap".len());
      let out = crate::interner::lookup(id).expect("interned string should be present");
      // Safety: `lookup` returns a valid byte slice for the returned length.
      let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
      assert_eq!(bytes, b"zap");
    });
  }

  #[test]
  fn interning_is_thread_safe() {
    crate::interner::with_test_lock(|| {
      const THREADS: usize = 8;
      const ITERS: usize = 1000;

      let mut handles = Vec::new();
      for _ in 0..THREADS {
        handles.push(std::thread::spawn(|| {
          let mut last = None;
          for _ in 0..ITERS {
            let id = rt_string_intern(b"concurrent".as_ptr(), b"concurrent".len());
            if let Some(prev) = last {
              assert_eq!(prev, id);
            }
            last = Some(id);
          }
          last.unwrap()
        }));
      }

      let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
      for &id in &ids[1..] {
        assert_eq!(ids[0], id);
      }
    });
  }

  #[test]
  fn interner_prunes_unpinned_strings_but_keeps_pinned() {
    crate::interner::with_test_lock(|| {
      let id_unpinned = rt_string_intern(b"temp".as_ptr(), b"temp".len());
      let id_pinned = rt_string_intern(b"perm".as_ptr(), b"perm".len());

      rt_string_pin_interned(id_pinned);

      // Force a GC sweep of the interner's backing heap. Since the interner keeps only weak
      // references to non-pinned entries, they should be collected and pruned.
      crate::interner::collect_garbage_for_tests();

      assert!(crate::interner::lookup(id_unpinned).is_none());

      let out = crate::interner::lookup(id_pinned).expect("pinned interned string should remain");
      // Safety: `lookup` returns a valid byte slice for the returned length.
      let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
      assert_eq!(bytes, b"perm");

      // Re-interning a collected string yields a new ID (IDs are never reused).
      let id_unpinned_2 = rt_string_intern(b"temp".as_ptr(), b"temp".len());
      assert_ne!(id_unpinned, id_unpinned_2);

      // Pinned strings remain stable.
      let id_pinned_2 = rt_string_intern(b"perm".as_ptr(), b"perm".len());
      assert_eq!(id_pinned, id_pinned_2);
    });
  }

  #[test]
  fn type_descriptor_ptr_offsets_are_from_object_base() {
    // This is a "sanity check" test that encodes the contract documented in
    // `vendor/ecma-rs/docs/runtime-native.md`:
    // - `TypeDescriptor.size` includes the `ObjHeader`
    // - `TypeDescriptor.ptr_offsets[]` are byte offsets from the object base
    //   pointer (the start of the `ObjHeader`), not from a payload pointer.
    const DOCS: &str = include_str!("../../docs/runtime-native.md");
    assert!(
      DOCS.contains("object base pointer"),
      "docs should describe base-pointer object references"
    );
    assert!(
      !DOCS.contains("returns a pointer to the start of the *payload*"),
      "docs must not describe payload-pointer object references"
    );

    #[repr(C)]
    struct DummyPayload {
      non_ptr0: usize,
      ptr0: *mut u8,
      non_ptr1: u32,
      ptr1: *mut u8,
    }

    const PTR_OFFSETS: [u32; 2] = [
      (crate::gc::OBJ_HEADER_SIZE + std::mem::offset_of!(DummyPayload, ptr0)) as u32,
      (crate::gc::OBJ_HEADER_SIZE + std::mem::offset_of!(DummyPayload, ptr1)) as u32,
    ];

    const TOTAL_SIZE: usize = crate::gc::OBJ_HEADER_SIZE + std::mem::size_of::<DummyPayload>();
    static DESC: crate::gc::TypeDescriptor = crate::gc::TypeDescriptor::new(TOTAL_SIZE, &PTR_OFFSETS);

    #[repr(C)]
    struct DummyObject {
      header: crate::gc::ObjHeader,
      payload: DummyPayload,
    }

    let mut obj = DummyObject {
      header: crate::gc::ObjHeader {
        type_desc: &DESC,
        meta: std::sync::atomic::AtomicUsize::new(0),
      },
      payload: DummyPayload {
        non_ptr0: 123,
        ptr0: 0x1111usize as *mut u8,
        non_ptr1: 456,
        ptr1: 0x2222usize as *mut u8,
      },
    };

    assert_eq!(DESC.size, std::mem::size_of::<DummyObject>());

    let obj_base = (&mut obj as *mut DummyObject) as *mut u8;

    // Verify that the computed pointer offsets match the actual field locations.
    let ptr0_addr = (&mut obj.payload.ptr0 as *mut *mut u8) as usize;
    let ptr1_addr = (&mut obj.payload.ptr1 as *mut *mut u8) as usize;
    let base_addr = obj_base as usize;
    assert_eq!((ptr0_addr - base_addr) as u32, PTR_OFFSETS[0]);
    assert_eq!((ptr1_addr - base_addr) as u32, PTR_OFFSETS[1]);

    // And ensure `for_each_ptr_slot` visits the right addresses given the base pointer.
    let mut seen = Vec::<usize>::new();
    unsafe {
      crate::gc::for_each_ptr_slot(obj_base, |slot| {
        seen.push(slot as usize);
      });
    }
    assert_eq!(seen, vec![ptr0_addr, ptr1_addr]);
  }

  #[test]
  fn c_header_matches_exported_entrypoints() {
    const HEADER: &str = include_str!("../include/runtime_native.h");

    // Keep these strings in sync with `include/runtime_native.h` to ensure we
    // don't forget to update the header when changing the exported ABI.
    const DECLS: &[&str] = &[
      "typedef uint8_t* GcPtr;",
      "typedef uint8_t** GcHandle;",
      "void rt_thread_init(uint32_t kind);",
      "void rt_thread_deinit(void);",
      "void rt_register_current_thread(void);",
      "void rt_unregister_current_thread(void);",
      "Thread* rt_thread_attach(Runtime* runtime);",
      "void rt_thread_detach(Thread* thread);",
      "GcPtr rt_alloc(size_t size, RtShapeId shape);",
      "GcPtr rt_alloc_pinned(size_t size, RtShapeId shape);",
      "GcPtr rt_alloc_array(size_t len, size_t elem_size);",
      "void rt_register_shape_table(const RtShapeDescriptor* table, size_t len);",
      "bool rt_gc_poll(void);",
      "void rt_gc_safepoint(void);",
      "GcPtr rt_gc_safepoint_relocate_h(GcHandle slot);",
      "void rt_gc_safepoint_slow(uint64_t epoch);",
      "bool rt_gc_poll(void);",
      "void rt_write_barrier(GcPtr obj, uint8_t* slot);",
      "void rt_write_barrier_range(GcPtr obj, uint8_t* start_slot, size_t len);",
      "void rt_gc_collect(void);",
      "size_t rt_backing_store_external_bytes(void);",
      "void rt_root_push(GcHandle slot);",
      "void rt_root_pop(GcHandle slot);",
      "uint32_t rt_gc_register_root_slot(GcHandle slot);",
      "void rt_gc_unregister_root_slot(uint32_t handle);",
      "uint32_t rt_gc_pin(GcPtr ptr);",
      "void rt_gc_unpin(uint32_t handle);",
      "void rt_gc_set_young_range(uint8_t* start, uint8_t* end);",
      "void rt_gc_get_young_range(GcPtr* out_start, GcPtr* out_end);",
      "uint64_t rt_weak_add(GcPtr value);",
      "GcPtr rt_weak_get(uint64_t handle);",
      "void rt_weak_remove(uint64_t handle);",
      "uint64_t rt_thread_register(uint32_t kind);",
      "void rt_thread_unregister(void);",
      "void rt_thread_set_parked(bool parked);",
      "StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);",
      "InternedId rt_string_intern(const uint8_t* s, size_t len);",
      "void rt_string_pin_interned(InternedId id);",
      "TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);",
      "void rt_parallel_join(const TaskId* tasks, size_t count);",
      "void rt_parallel_for(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);",
      "PromiseRef rt_parallel_spawn_promise(void (*task)(uint8_t*, PromiseRef), uint8_t* data, PromiseLayout layout);",
      "LegacyPromiseRef rt_spawn_blocking(void (*task)(uint8_t*, LegacyPromiseRef), uint8_t* data);",
      "void rt_promise_init(PromiseRef p);",
      "void rt_promise_fulfill(PromiseRef p);",
      "void rt_promise_reject(PromiseRef p);",
      "uint8_t* rt_promise_payload_ptr(PromiseRef p);",
      "PromiseRef rt_async_spawn(CoroutineRef coro);",
      "void rt_async_cancel_all(void);",
      "bool rt_async_poll(void);",
      "void rt_async_wait(void);",
      "void rt_async_set_strict_await_yields(bool strict);",
      "bool rt_async_run_until_idle(void);",
      "void rt_async_block_on(PromiseRef p);",
      "LegacyPromiseRef rt_promise_new_legacy(void);",
      "void rt_promise_resolve_legacy(LegacyPromiseRef p, ValueRef value);",
      "void rt_promise_reject_legacy(LegacyPromiseRef p, ValueRef err);",
      "void rt_promise_then_legacy(LegacyPromiseRef p, void (*on_settle)(uint8_t*), uint8_t* data);",
      "LegacyPromiseRef rt_async_spawn_legacy(RtCoroutineHeader* coro);",
      "bool rt_async_poll_legacy(void);",
      "LegacyPromiseRef rt_async_sleep_legacy(uint64_t delay_ms);",
      "void rt_queue_microtask(void (*cb)(uint8_t*), uint8_t* data);",
      "TimerId rt_set_timeout(void (*cb)(uint8_t*), uint8_t* data, uint64_t delay_ms);",
      "TimerId rt_set_interval(void (*cb)(uint8_t*), uint8_t* data, uint64_t interval_ms);",
      "void rt_clear_timer(TimerId id);",
      "void rt_coro_await_legacy(RtCoroutineHeader* coro, LegacyPromiseRef awaited, uint32_t next_state);",
    ];

    for decl in DECLS {
      assert!(
        HEADER.contains(decl),
        "`runtime_native.h` is missing expected declaration: {decl}"
      );
    }

    if cfg!(feature = "gc_stats") {
      for decl in ["void rt_gc_stats_snapshot(RtGcStatsSnapshot* out);", "void rt_gc_stats_reset(void);"] {
        assert!(
          HEADER.contains(decl),
          "`runtime_native.h` is missing expected gc_stats declaration: {decl}"
        );
      }
    }

    // Ensure the Rust exports match the declared ABI shape.
    let _thread_init: extern "C" fn(u32) = rt_thread_init;
    let _thread_deinit: extern "C" fn() = rt_thread_deinit;
    let _register_current: extern "C" fn() = rt_register_current_thread;
    let _unregister_current: extern "C" fn() = rt_unregister_current_thread;
    let _thread_attach: unsafe extern "C" fn(*mut Runtime) -> *mut Thread = rt_thread_attach;
    let _thread_detach: unsafe extern "C" fn(*mut Thread) = rt_thread_detach;
    let _alloc: extern "C" fn(usize, abi::RtShapeId) -> *mut u8 = rt_alloc;
    let _alloc_pinned: extern "C" fn(usize, abi::RtShapeId) -> *mut u8 = rt_alloc_pinned;
    let _alloc_array: extern "C" fn(usize, usize) -> *mut u8 = rt_alloc_array;
    let _register_shape_table: unsafe extern "C" fn(*const abi::RtShapeDescriptor, usize) =
      crate::shape_table::rt_register_shape_table;
    let _gc_poll: extern "C" fn() -> bool = rt_gc_poll;
    let _safepoint: extern "C" fn() = rt_gc_safepoint;
    let _slow: unsafe extern "C" fn(u64) = rt_gc_safepoint_slow;
    let _gc_poll: extern "C" fn() -> bool = rt_gc_poll;
    let _write_barrier: unsafe extern "C" fn(*mut u8, *mut u8) = rt_write_barrier;
    let _write_barrier_range: unsafe extern "C" fn(*mut u8, *mut u8, usize) = rt_write_barrier_range;
    let _collect: extern "C" fn() = rt_gc_collect;
    let _set_young_range: extern "C" fn(*mut u8, *mut u8) = rt_gc_set_young_range;
    let _get_young_range: unsafe extern "C" fn(*mut *mut u8, *mut *mut u8) = rt_gc_get_young_range;
    let _weak_add: extern "C" fn(*mut u8) -> u64 = rt_weak_add;
    let _weak_get: extern "C" fn(u64) -> *mut u8 = rt_weak_get;
    let _weak_remove: extern "C" fn(u64) = rt_weak_remove;
    let _root_push: unsafe extern "C" fn(crate::roots::GcHandle) = rt_root_push;
    let _root_pop: unsafe extern "C" fn(crate::roots::GcHandle) = rt_root_pop;
    let _thread_register: extern "C" fn(u32) -> u64 = rt_thread_register;
    let _thread_unregister: extern "C" fn() = rt_thread_unregister;
    let _thread_set_parked: extern "C" fn(bool) = rt_thread_set_parked;
    let _concat: extern "C" fn(*const u8, usize, *const u8, usize) -> abi::StringRef = rt_string_concat;
    let _intern: extern "C" fn(*const u8, usize) -> abi::InternedId = rt_string_intern;
    let _pin_interned: extern "C" fn(abi::InternedId) = rt_string_pin_interned;
    let _spawn: extern "C" fn(extern "C" fn(*mut u8), *mut u8) -> abi::TaskId = rt_parallel_spawn;
    let _join: extern "C" fn(*const abi::TaskId, usize) = rt_parallel_join;
    let _for: extern "C" fn(usize, usize, extern "C" fn(usize, *mut u8), *mut u8) = rt_parallel_for;
    let _spawn_promise: extern "C" fn(extern "C" fn(*mut u8, abi::PromiseRef), *mut u8, PromiseLayout) -> abi::PromiseRef =
      rt_parallel_spawn_promise;
    let _spawn_blocking: extern "C" fn(extern "C" fn(*mut u8, abi::PromiseRef), *mut u8) -> abi::PromiseRef =
      rt_spawn_blocking;
    let _promise_init: unsafe extern "C" fn(PromiseRef) = rt_promise_init;
    let _promise_fulfill: unsafe extern "C" fn(PromiseRef) = rt_promise_fulfill;
    let _promise_reject: unsafe extern "C" fn(PromiseRef) = rt_promise_reject;
    let _promise_payload_ptr: extern "C" fn(PromiseRef) -> *mut u8 = rt_promise_payload_ptr;
    let _async_spawn: unsafe extern "C" fn(CoroutineRef) -> PromiseRef = rt_async_spawn;
    let _async_cancel_all: extern "C" fn() = rt_async_cancel_all;
    let _async_poll: extern "C" fn() -> bool = rt_async_poll;
    let _async_wait: extern "C" fn() = rt_async_wait;
    let _async_set_strict_await_yields: extern "C" fn(bool) = rt_async_set_strict_await_yields;
    let _async_run_until_idle: unsafe extern "C" fn() -> bool = rt_async_run_until_idle_abi;
    let _async_block_on: unsafe extern "C" fn(PromiseRef) = rt_async_block_on;
    let _promise_new_legacy: extern "C" fn() -> abi::PromiseRef = rt_promise_new_legacy;
    let _promise_resolve_legacy: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_resolve_legacy;
    let _promise_reject_legacy: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_reject_legacy;
    let _promise_then_legacy: extern "C" fn(abi::PromiseRef, extern "C" fn(*mut u8), *mut u8) = rt_promise_then_legacy;
    let _async_spawn_legacy: extern "C" fn(*mut abi::RtCoroutineHeader) -> abi::PromiseRef = rt_async_spawn_legacy;
    let _async_poll_legacy: extern "C" fn() -> bool = rt_async_poll_legacy;
    let _async_sleep_legacy: extern "C" fn(u64) -> abi::PromiseRef = rt_async_sleep_legacy;
    let _queue_microtask: extern "C" fn(extern "C" fn(*mut u8), *mut u8) = rt_queue_microtask;
    let _set_timeout: extern "C" fn(extern "C" fn(*mut u8), *mut u8, u64) -> abi::TimerId = rt_set_timeout;
    let _set_interval: extern "C" fn(extern "C" fn(*mut u8), *mut u8, u64) -> abi::TimerId = rt_set_interval;
    let _clear_timer: extern "C" fn(abi::TimerId) = rt_clear_timer;
    let _coro_await_legacy: extern "C" fn(*mut abi::RtCoroutineHeader, abi::PromiseRef, u32) = rt_coro_await_legacy;

    #[cfg(feature = "gc_stats")]
    let _stats_snapshot: unsafe extern "C" fn(*mut abi::RtGcStatsSnapshot) = rt_gc_stats_snapshot;
    #[cfg(feature = "gc_stats")]
    let _stats_reset: extern "C" fn() = rt_gc_stats_reset;
    #[cfg(feature = "gc_stats")]
    let _ = (_stats_snapshot, _stats_reset);

    let _ = (
      _thread_init,
      _thread_deinit,
      _register_current,
      _unregister_current,
      _thread_attach,
      _thread_detach,
      _alloc,
      _alloc_pinned,
      _alloc_array,
      _register_shape_table,
      _gc_poll,
      _safepoint,
      _slow,
      _gc_poll,
      _write_barrier,
      _write_barrier_range,
      _collect,
      _set_young_range,
      _get_young_range,
      _weak_add,
      _weak_get,
      _weak_remove,
      _root_push,
      _root_pop,
      _thread_register,
      _thread_unregister,
      _thread_set_parked,
      _concat,
      _intern,
      _pin_interned,
      _spawn,
      _join,
      _for,
      _spawn_promise,
      _spawn_blocking,
      _promise_init,
      _promise_fulfill,
      _promise_reject,
      _promise_payload_ptr,
      _async_spawn,
      _async_cancel_all,
      _async_poll,
      _async_wait,
      _async_set_strict_await_yields,
      _async_run_until_idle,
      _async_block_on,
      _promise_new_legacy,
      _promise_resolve_legacy,
      _promise_reject_legacy,
      _promise_then_legacy,
      _async_spawn_legacy,
      _async_poll_legacy,
      _async_sleep_legacy,
      _queue_microtask,
      _set_timeout,
      _set_interval,
      _clear_timer,
      _coro_await_legacy,
    );
  }
  #[test]
  fn stackmaps_section_is_accessible_and_parses() {
    let bytes = stackmaps_section();
    #[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
    assert!(
      !bytes.is_empty(),
      "expected .llvm_stackmaps to be linked in (test includes a dummy section)"
    );
    if bytes.is_empty() {
      // This loader is opt-in: when the `llvm_stackmaps_linker` feature is disabled (or stackmaps
      // are not present on this platform), we intentionally treat the stackmaps section as
      // unavailable.
      return;
    }

    let parsed = StackMaps::parse(bytes).expect("stack maps should parse");
    assert_eq!(parsed.raw().version, 3);

    // Also validate the lazy global accessor used by stack walking / GC.
    let cached = crate::stackmap::stackmaps();
    assert_eq!(cached.raw().version, 3);
  }

  extern "C" fn inc_atomic(data: *mut u8) {
    let atomic = unsafe { &*(data as *const AtomicUsize) };
    atomic.fetch_add(1, Ordering::Relaxed);
  }

  #[test]
  fn parallel_spawn_and_join_increments_atomic() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    let counter = AtomicUsize::new(0);
    let counter_ptr = (&counter as *const AtomicUsize).cast_mut().cast::<u8>();

    let task_count = 128;
    let mut tasks: Vec<abi::TaskId> = Vec::with_capacity(task_count);
    for _ in 0..task_count {
      let task = rt_parallel_spawn(inc_atomic, counter_ptr);
      tasks.push(task);
    }

    rt_parallel_join(tasks.as_ptr(), tasks.len());

    assert_eq!(counter.load(Ordering::Relaxed), task_count);
  }

  #[repr(C)]
  struct CounterTaskData {
    counters: *const AtomicUsize,
    idx: usize,
  }

  extern "C" fn bump_indexed_counter(data: *mut u8) {
    let data = unsafe { Box::from_raw(data as *mut CounterTaskData) };
    let counter = unsafe { &*data.counters.add(data.idx) };
    counter.fetch_add(1, Ordering::Relaxed);
  }

  #[repr(C)]
  struct NestedSpawnCtx {
    counters: *const AtomicUsize,
    n_children: usize,
  }

  extern "C" fn nested_parent_task(data: *mut u8) {
    let ctx = unsafe { Box::from_raw(data as *mut NestedSpawnCtx) };

    let mut tasks: Vec<abi::TaskId> = Vec::with_capacity(ctx.n_children);
    for idx in 0..ctx.n_children {
      let child = Box::new(CounterTaskData {
        counters: ctx.counters,
        idx,
      });
      tasks.push(rt_parallel_spawn(
        bump_indexed_counter,
        Box::into_raw(child).cast::<u8>(),
      ));
    }
    rt_parallel_join(tasks.as_ptr(), tasks.len());
  }

  #[test]
  fn parallel_spawn_join_can_spawn_from_worker_thread() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    const CHILDREN: usize = 4096;
    let counters: Vec<AtomicUsize> = (0..CHILDREN).map(|_| AtomicUsize::new(0)).collect();

    let ctx = Box::new(NestedSpawnCtx {
      counters: counters.as_ptr(),
      n_children: CHILDREN,
    });
    let parent = rt_parallel_spawn(nested_parent_task, Box::into_raw(ctx).cast::<u8>());
    rt_parallel_join(&parent as *const _, 1);

    for (idx, counter) in counters.iter().enumerate() {
      assert_eq!(
        counter.load(Ordering::Relaxed),
        1,
        "nested task {idx} ran unexpected number of times"
      );
    }
  }

  #[test]
  fn parallel_spawn_stress_runs_each_task_exactly_once() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    const TASKS: usize = 50_000;

    let counters: Vec<AtomicUsize> = (0..TASKS).map(|_| AtomicUsize::new(0)).collect();
    let mut tasks: Vec<abi::TaskId> = Vec::with_capacity(TASKS);
    for idx in 0..TASKS {
      let data = Box::new(CounterTaskData {
        counters: counters.as_ptr(),
        idx,
      });
      tasks.push(rt_parallel_spawn(
        bump_indexed_counter,
        Box::into_raw(data).cast::<u8>(),
      ));
    }

    rt_parallel_join(tasks.as_ptr(), tasks.len());

    for (idx, counter) in counters.iter().enumerate() {
      assert_eq!(
        counter.load(Ordering::Relaxed),
        1,
        "task {idx} ran unexpected number of times"
      );
    }
  }

  #[repr(C)]
  struct FillData {
    ptr: *mut u8,
    len: usize,
    value: u8,
  }

  extern "C" fn fill_slice(data: *mut u8) {
    let data = unsafe { &*(data as *const FillData) };
    unsafe {
      std::slice::from_raw_parts_mut(data.ptr, data.len).fill(data.value);
    }
  }

  #[test]
  fn parallel_tasks_write_to_disjoint_slices() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    let mut out = vec![0u8; 1024];

    let mut task_ids: Vec<abi::TaskId> = Vec::new();
    let mut task_data: Vec<Box<FillData>> = Vec::new();

    // Split the output into equal segments and fill each with a unique value.
    let segments = 16usize;
    let seg_len = out.len() / segments;
    for i in 0..segments {
      let start = i * seg_len;
      let len = if i == segments - 1 {
        out.len() - start
      } else {
        seg_len
      };

      let data = Box::new(FillData {
        ptr: unsafe { out.as_mut_ptr().add(start) },
        len,
        value: (i as u8).wrapping_add(1),
      });
      let data_ptr = (&*data as *const FillData).cast_mut().cast::<u8>();

      let task = rt_parallel_spawn(fill_slice, data_ptr);
      task_ids.push(task);
      task_data.push(data);
    }

    rt_parallel_join(task_ids.as_ptr(), task_ids.len());

    for i in 0..segments {
      let start = i * seg_len;
      let len = if i == segments - 1 {
        out.len() - start
      } else {
        seg_len
      };
      let expected = (i as u8).wrapping_add(1);
      assert!(out[start..start + len].iter().all(|&b| b == expected));
    }

    drop(task_data);
  }

  #[test]
  fn parallel_join_empty_is_noop() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    rt_parallel_join(std::ptr::null(), 0);
  }

  fn effective_worker_count() -> usize {
    std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS")
      .ok()
      .and_then(|v| v.parse::<usize>().ok())
      .filter(|&n| n > 0)
      .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
  }

  #[repr(C)]
  struct ConcurrencyData {
    active: AtomicUsize,
    max_active: AtomicUsize,
    release: AtomicBool,
  }

  extern "C" fn concurrency_probe(data: *mut u8) {
    let data = unsafe { &*(data as *const ConcurrencyData) };

    let active = data.active.fetch_add(1, Ordering::SeqCst) + 1;
    data.max_active.fetch_max(active, Ordering::SeqCst);

    // Keep the task alive until the test releases it so other workers have a chance to overlap.
    let start = Instant::now();
    while !data.release.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
      std::thread::yield_now();
    }

    data.active.fetch_sub(1, Ordering::SeqCst);
  }

  #[test]
  fn parallel_spawn_can_execute_concurrently() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    // This test is a best-effort concurrency probe: if the pool only has 1 worker, we can't
    // reliably assert overlap across OS threads.
    let workers = effective_worker_count();
    if workers < 2 {
      return;
    }

    let data = Box::new(ConcurrencyData {
      active: AtomicUsize::new(0),
      max_active: AtomicUsize::new(0),
      release: AtomicBool::new(false),
    });
    let data_ptr = (&*data as *const ConcurrencyData).cast_mut().cast::<u8>();

    let tasks = [
      rt_parallel_spawn(concurrency_probe, data_ptr),
      rt_parallel_spawn(concurrency_probe, data_ptr),
    ];
    // Worker threads are spawned lazily on first use. Avoid calling `rt_parallel_join` immediately:
    // the joiner thread can legitimately execute tasks itself, which makes overlap a race. Give
    // the worker pool time to start both tasks so we can observe overlap reliably.
    let start = Instant::now();
    while data.max_active.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(2) {
      std::thread::yield_now();
    }
    data.release.store(true, Ordering::SeqCst);
    rt_parallel_join(tasks.as_ptr(), tasks.len());

    assert!(
      data.max_active.load(Ordering::SeqCst) >= 2,
      "expected at least two tasks to overlap; worker_count={workers}"
    );
  }

  #[repr(C)]
  struct Leaf {
    header: crate::gc::ObjHeader,
  }

  static LEAF_DESC: crate::gc::TypeDescriptor = crate::gc::TypeDescriptor::new(
    core::mem::size_of::<Leaf>(),
    &[],
  );

  #[test]
  fn gc_traces_pointer_arrays_in_minor_gc() {
    let mut heap = GcHeap::new();

    let a = heap.alloc_young(&LEAF_DESC);
    let b = heap.alloc_young(&LEAF_DESC);
    let array = heap.alloc_array_young(2, core::mem::size_of::<*mut u8>() | array::RT_ARRAY_ELEM_PTR_FLAG);

    // Store pointers to `a` and `b` in the array payload.
    let elems = unsafe { array::array_data_ptr(array).cast::<*mut u8>() };
    unsafe {
      elems.add(0).write(a);
      elems.add(1).write(b);
    }

    let mut root_array = array;
    let mut roots = RootStack::new();
    roots.push(&mut root_array as *mut *mut u8);
    let mut remembered = crate::gc::SimpleRememberedSet::new();

    heap.collect_minor(&mut roots, &mut remembered);

    assert!(!heap.is_in_nursery(root_array));

    let elems = unsafe { array::array_data_ptr(root_array).cast::<*mut u8>() };
    let a2 = unsafe { elems.add(0).read() };
    let b2 = unsafe { elems.add(1).read() };
    assert!(!heap.is_in_nursery(a2), "array element should be promoted out of nursery");
    assert!(!heap.is_in_nursery(b2), "array element should be promoted out of nursery");
    assert!(heap.is_in_immix(a2));
    assert!(heap.is_in_immix(b2));
  }

  #[test]
  fn gc_does_not_trace_ptr_sized_byte_arrays_in_minor_gc() {
    let mut heap = GcHeap::new();

    let obj = heap.alloc_young(&LEAF_DESC);
    // Pointer-sized elements but *no* RT_ARRAY_ELEM_PTR_FLAG => raw bytes.
    let array = heap.alloc_array_young(1, core::mem::size_of::<*mut u8>());

    // Stash the object pointer bits into the array payload.
    let slot = unsafe { array::array_data_ptr(array) as *mut *mut u8 };
    unsafe {
      slot.write(obj);
    }

    let mut root_array = array;
    let mut roots = RootStack::new();
    roots.push(&mut root_array as *mut *mut u8);
    let mut remembered = crate::gc::SimpleRememberedSet::new();

    heap.collect_minor(&mut roots, &mut remembered);

    // If the GC incorrectly treated the payload as pointer slots, it would have evacuated `obj`
    // and updated the stored value to point to the promoted object (outside the nursery).
    let slot = unsafe { array::array_data_ptr(root_array) as *const *mut u8 };
    let stored = unsafe { slot.read() };
    assert!(
      heap.is_in_nursery(stored),
      "raw bytes must not be treated as GC pointers"
    );
  }

  #[test]
  fn array_size_overflow_is_detected() {
    assert!(array::decode_rt_array_elem_size(array::RT_ARRAY_ELEM_PTR_FLAG | 4).is_none());
    assert!(array::checked_total_bytes(usize::MAX, 16).is_none());
  }

  #[derive(Default)]
  struct NullRememberedSet;

  impl RememberedSet for NullRememberedSet {
    fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}

    fn clear(&mut self) {}

    fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
  }

  #[repr(C)]
  struct Node {
    _header: crate::gc::ObjHeader,
    left: *mut u8,
    right: *mut u8,
    value: usize,
  }

  const NODE_PTR_OFFSETS: [u32; 2] = [
    std::mem::offset_of!(Node, left) as u32,
    std::mem::offset_of!(Node, right) as u32,
  ];

  static NODE_DESC: TypeDescriptor = TypeDescriptor::new(std::mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

  #[repr(C)]
  struct BigNode {
    _header: crate::gc::ObjHeader,
    next: *mut u8,
    payload: [u8; 32],
  }

  const BIG_NODE_PTR_OFFSETS: [u32; 1] = [std::mem::offset_of!(BigNode, next) as u32];

  static BIG_NODE_DESC: TypeDescriptor = TypeDescriptor::new(std::mem::size_of::<BigNode>(), &BIG_NODE_PTR_OFFSETS);

  #[test]
  fn major_gc_traces_by_type_descriptor_and_reclaims_unreachable() {
    let mut heap = GcHeap::new();
    let mut roots = RootStack::new();
    let mut remembered = NullRememberedSet::default();

    let big1 = heap.alloc_pinned(&BIG_NODE_DESC);
    let big0 = heap.alloc_pinned(&BIG_NODE_DESC);
    let node = heap.alloc_old(&NODE_DESC);

    // SAFETY: `alloc_*` returns valid, properly-sized objects, and the types
    // here are `#[repr(C)]` with `ObjHeader` as the first field.
    unsafe {
      (*(big0 as *mut BigNode)).next = big1;

      (*(node as *mut Node)).left = big0;
      (*(node as *mut Node)).right = std::ptr::null_mut();
      (*(node as *mut Node)).value = 123;
    }

    let mut root = node;
    roots.push(&mut root as *mut *mut u8);

    heap.collect_major(&mut roots, &mut remembered);
    assert_eq!(heap.los_object_count(), 2);

    // Ensure live objects remain intact after a major GC and that the collector
    // follows pointer fields described by [`TypeDescriptor`].
    let node = root as *mut Node;
    unsafe {
      assert_eq!((*node).value, 123);
      let left = (*node).left;
      assert!(!left.is_null());
      assert_eq!((*(left as *mut BigNode)).next, big1);
    }

    // Drop the only reference to the LOS object chain and ensure it is swept.
    unsafe {
      (*(root as *mut Node)).left = std::ptr::null_mut();
    }
    heap.collect_major(&mut roots, &mut remembered);
    assert_eq!(heap.los_object_count(), 0);

    // RootStack must support explicit pop for callers that manage stack
    // discipline manually.
    let popped = roots.pop();
    assert_eq!(popped, (&mut root as *mut *mut u8));
    root = std::ptr::null_mut();
    assert!(root.is_null());

    heap.collect_major(&mut roots, &mut remembered);
  }

  #[test]
  fn repeated_major_collections_make_progress() {
    let mut heap = GcHeap::new();
    let mut roots = RootStack::new();
    let mut remembered = NullRememberedSet::default();

    for _ in 0..32 {
      let obj = heap.alloc_pinned(&BIG_NODE_DESC);
      let mut root = obj;
      roots.push(&mut root as *mut *mut u8);

      heap.collect_major(&mut roots, &mut remembered);
      assert_eq!(heap.los_object_count(), 1);

      roots.pop();
      root = std::ptr::null_mut();
      assert!(root.is_null());

      heap.collect_major(&mut roots, &mut remembered);
      assert_eq!(heap.los_object_count(), 0);
    }
  }

  const CHILD_ENV: &str = "ECMA_RS_RUNTIME_NATIVE_PANIC_BOUNDARY_CHILD";

  #[test]
  fn exported_ffi_functions_abort_on_panic() {
    if std::env::var_os(CHILD_ENV).is_some() {
      rt_async_test_panic();
      unreachable!("rt_async_test_panic must abort the process");
    }

    let exe = std::env::current_exe().expect("failed to get current test executable path");
    let status = std::process::Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg("tests::exported_ffi_functions_abort_on_panic")
      .status()
      .expect("failed to spawn child test process");

    assert!(!status.success());

    #[cfg(unix)]
    {
      use std::os::unix::process::ExitStatusExt;
      assert_eq!(status.signal(), Some(6));
    }
  }
}

// Exported functions used by `tests/frame_pointers.rs` to validate the
// frame-pointer ABI contract in optimized builds.
//
// Keep these behind a feature so they don't become part of the default ABI
// surface.
#[cfg(feature = "fp_regression")]
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_fp_test_leaf(x: u64) -> u64 {
  x.wrapping_add(1)
}

#[cfg(feature = "fp_regression")]
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_fp_test_mid(x: u64) -> u64 {
  // Ensure a real call so we get a distinct frame in the disassembly.
  rt_fp_test_leaf(x).wrapping_mul(3)
}

#[cfg(feature = "fp_regression")]
#[no_mangle]
#[inline(never)]
pub extern "C" fn rt_fp_test_entry(x: u64) -> u64 {
  rt_fp_test_mid(x).wrapping_sub(7)
}
