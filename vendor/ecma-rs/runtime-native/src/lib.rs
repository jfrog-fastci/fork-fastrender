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
//! See:
//! - `docs/write_barrier.md` for the generational GC write barrier contract.
//! - `include/runtime_native.h` for the authoritative stable C ABI surface.

pub mod abi;
pub mod arch;
pub mod gc_safe;
pub mod async_rt;
pub mod gc;
pub mod immix;
pub mod los;
pub mod nursery;
pub mod stackmap;
pub mod parallel;
pub mod sync;
pub mod threading;
pub mod runtime;
pub mod thread;
pub mod stackmaps;
pub mod stackmaps_loader;
pub mod statepoints;
pub mod stackwalk;
pub mod stackwalk_fp;
pub mod test_util;
pub mod statepoint_verify;

// Core object model used by the planned Immix + generational collector.
pub mod metadata;
pub mod object;
pub mod shape_table;

mod alloc;
#[cfg(feature = "gc_stats")]
mod gc_stats;
mod blocking_pool;
mod exports;
mod interner;
mod platform;
mod string;
mod trap;

pub use exports::*;
pub use gc::GcHeap;
pub use gc::RememberedSet;
pub use gc::RootSet;
pub use gc::RootStack;
pub use gc::TypeDescriptor;
pub use async_rt::set_strict_await_yields;
pub use stackmaps::StackMaps;
pub use stackwalk_fp::{walk_gc_roots_from_fp, WalkError};
pub use string::*;
pub use stackmaps_loader::{load_stackmaps_from_self, stackmaps_section};
pub use runtime::{AttachError, DetachError, Runtime, StopTheWorldGuard, ThreadGuard};
pub use thread::{
  current_thread, current_thread_mut, current_thread_ptr, current_thread_state, Thread, ThreadState, RT_THREAD,
};

use std::sync::OnceLock;

struct GlobalRuntime {
  parallel: parallel::ParallelRuntime,
}

static RUNTIME: OnceLock<GlobalRuntime> = OnceLock::new();

fn rt_ensure_init() -> &'static GlobalRuntime {
  RUNTIME.get_or_init(|| GlobalRuntime {
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

// Unit tests need a `.llvm_stackmaps` section to validate the linker-script
// based loader without requiring LLVM statepoints in the Rust code.
#[cfg(all(test, target_os = "linux"))]
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
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::{Duration, Instant};

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
  fn gc_safepoint_poll_symbol_is_exported() {
    extern "C" {
      #[link_name = "gc.safepoint_poll"]
      fn gc_safepoint_poll();
    }

    // Safety: the symbol is exported by this crate and is safe to call. When no
    // stop-the-world GC is requested, the fast path returns immediately.
    unsafe {
      gc_safepoint_poll();
    }
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
        meta: 0,
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
      "void rt_thread_init(uint32_t kind);",
      "void rt_thread_deinit(void);",
      "Thread* rt_thread_attach(Runtime* runtime);",
      "void rt_thread_detach(Thread* thread);",
      "uint8_t* rt_alloc(size_t size, RtShapeId shape);",
      "uint8_t* rt_alloc_pinned(size_t size, RtShapeId shape);",
      "uint8_t* rt_alloc_array(size_t len, size_t elem_size);",
      "void rt_register_shape_table(const RtShapeDescriptor* table, size_t len);",
      "void rt_gc_safepoint(void);",
      "void rt_write_barrier(uint8_t* obj, uint8_t* slot);",
      "void rt_write_barrier_range(uint8_t* obj, uint8_t* start_slot, size_t len);",
      "void rt_gc_collect(void);",
      "void rt_gc_set_young_range(uint8_t* start, uint8_t* end);",
      "void rt_gc_get_young_range(uint8_t** out_start, uint8_t** out_end);",
      "uint64_t rt_weak_add(uint8_t* value);",
      "uint8_t* rt_weak_get(uint64_t handle);",
      "void rt_weak_remove(uint64_t handle);",
      "StringRef rt_string_concat(const uint8_t* a, size_t a_len, const uint8_t* b, size_t b_len);",
      "InternedId rt_string_intern(const uint8_t* s, size_t len);",
      "TaskId rt_parallel_spawn(void (*task)(uint8_t*), uint8_t* data);",
      "void rt_parallel_join(const TaskId* tasks, size_t count);",
      "void rt_parallel_for(size_t start, size_t end, void (*body)(size_t, uint8_t*), uint8_t* data);",
      "PromiseRef rt_spawn_blocking(void (*task)(uint8_t*, PromiseRef), uint8_t* data);",
      "PromiseRef rt_async_spawn(RtCoroutineHeader* coro);",
      "bool rt_async_poll(void);",
      "PromiseRef rt_async_sleep(uint64_t delay_ms);",
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
    let _thread_attach: unsafe extern "C" fn(*mut Runtime) -> *mut Thread = rt_thread_attach;
    let _thread_detach: unsafe extern "C" fn(*mut Thread) = rt_thread_detach;
    let _alloc: extern "C" fn(usize, abi::RtShapeId) -> *mut u8 = rt_alloc;
    let _alloc_pinned: extern "C" fn(usize, abi::RtShapeId) -> *mut u8 = rt_alloc_pinned;
    let _alloc_array: extern "C" fn(usize, usize) -> *mut u8 = rt_alloc_array;
    let _register_shape_table: unsafe extern "C" fn(*const abi::RtShapeDescriptor, usize) =
      crate::shape_table::rt_register_shape_table;
    let _safepoint: extern "C" fn() = rt_gc_safepoint;
    let _write_barrier: unsafe extern "C" fn(*mut u8, *mut u8) = rt_write_barrier;
    let _write_barrier_range: unsafe extern "C" fn(*mut u8, *mut u8, usize) = rt_write_barrier_range;
    let _collect: extern "C" fn() = rt_gc_collect;
    let _set_young_range: extern "C" fn(*mut u8, *mut u8) = rt_gc_set_young_range;
    let _get_young_range: unsafe extern "C" fn(*mut *mut u8, *mut *mut u8) = rt_gc_get_young_range;
    let _weak_add: extern "C" fn(*mut u8) -> u64 = rt_weak_add;
    let _weak_get: extern "C" fn(u64) -> *mut u8 = rt_weak_get;
    let _weak_remove: extern "C" fn(u64) = rt_weak_remove;
    let _concat: extern "C" fn(*const u8, usize, *const u8, usize) -> abi::StringRef = rt_string_concat;
    let _intern: extern "C" fn(*const u8, usize) -> abi::InternedId = rt_string_intern;
    let _spawn: extern "C" fn(extern "C" fn(*mut u8), *mut u8) -> abi::TaskId = rt_parallel_spawn;
    let _join: extern "C" fn(*const abi::TaskId, usize) = rt_parallel_join;
    let _for: extern "C" fn(usize, usize, extern "C" fn(usize, *mut u8), *mut u8) = rt_parallel_for;
    let _spawn_blocking: extern "C" fn(extern "C" fn(*mut u8, abi::PromiseRef), *mut u8) -> abi::PromiseRef =
      rt_spawn_blocking;
    let _async_spawn: extern "C" fn(*mut abi::RtCoroutineHeader) -> abi::PromiseRef = rt_async_spawn;
    let _async_poll: extern "C" fn() -> bool = rt_async_poll;
    let _async_sleep: extern "C" fn(u64) -> abi::PromiseRef = rt_async_sleep;
    let _promise_new: extern "C" fn() -> abi::PromiseRef = rt_promise_new;
    let _promise_resolve: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_resolve;
    let _promise_reject: extern "C" fn(abi::PromiseRef, abi::ValueRef) = rt_promise_reject;
    let _promise_then: extern "C" fn(abi::PromiseRef, extern "C" fn(*mut u8), *mut u8) = rt_promise_then;
    let _coro_await: extern "C" fn(*mut abi::RtCoroutineHeader, abi::PromiseRef, u32) = rt_coro_await;

    #[cfg(feature = "gc_stats")]
    let _stats_snapshot: unsafe extern "C" fn(*mut abi::RtGcStatsSnapshot) = rt_gc_stats_snapshot;
    #[cfg(feature = "gc_stats")]
    let _stats_reset: extern "C" fn() = rt_gc_stats_reset;
    #[cfg(feature = "gc_stats")]
    let _ = (_stats_snapshot, _stats_reset);

    let _ = (
      _thread_init,
      _thread_deinit,
      _thread_attach,
      _thread_detach,
      _alloc,
      _alloc_pinned,
      _alloc_array,
      _register_shape_table,
      _safepoint,
      _write_barrier,
      _write_barrier_range,
      _collect,
      _set_young_range,
      _get_young_range,
      _weak_add,
      _weak_get,
      _weak_remove,
      _concat,
      _intern,
      _spawn,
      _join,
      _for,
      _spawn_blocking,
      _async_spawn,
      _async_poll,
      _async_sleep,
      _promise_new,
      _promise_resolve,
      _promise_reject,
      _promise_then,
      _coro_await,
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
      // Non-Linux builds don't have the linker-script based loader yet.
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
  }

  extern "C" fn concurrency_probe(data: *mut u8) {
    let data = unsafe { &*(data as *const ConcurrencyData) };

    let active = data.active.fetch_add(1, Ordering::SeqCst) + 1;
    data.max_active.fetch_max(active, Ordering::SeqCst);

    // Give another worker a generous window to overlap execution.
    if active == 1 {
      let start = Instant::now();
      while data.max_active.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_millis(500) {
        std::thread::yield_now();
      }
    }

    data.active.fetch_sub(1, Ordering::SeqCst);
  }

  #[test]
  fn parallel_spawn_can_execute_concurrently() {
    // This test is a best-effort concurrency probe: if the pool only has 1 worker, we can't
    // reliably assert overlap across OS threads.
    let workers = effective_worker_count();
    if workers < 2 {
      return;
    }

    let data = Box::new(ConcurrencyData {
      active: AtomicUsize::new(0),
      max_active: AtomicUsize::new(0),
    });
    let data_ptr = (&*data as *const ConcurrencyData).cast_mut().cast::<u8>();

    let tasks = [rt_parallel_spawn(concurrency_probe, data_ptr), rt_parallel_spawn(concurrency_probe, data_ptr)];
    rt_parallel_join(tasks.as_ptr(), tasks.len());

    assert!(
      data.max_active.load(Ordering::SeqCst) >= 2,
      "expected at least two tasks to overlap; worker_count={workers}"
    );
  }
}
