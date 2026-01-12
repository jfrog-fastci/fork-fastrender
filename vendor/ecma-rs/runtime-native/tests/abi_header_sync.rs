use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn runtime_native_c_header_contains_expected_abi_symbols() {
  let _rt = TestRuntimeGuard::new();
  const HEADER: &str = include_str!("../include/runtime_native.h");

  // Core barrier + GC range plumbing.
  for sym in [
    "rt_stackmaps_register(",
    "rt_stackmaps_unregister(",
    "rt_write_barrier(",
    "rt_write_barrier_range(",
    "rt_backing_store_external_bytes(",
    "rt_gc_register_root_slot(",
    "rt_gc_unregister_root_slot(",
    "rt_gc_pin(",
    "rt_gc_pin_h(",
    "rt_gc_unpin(",
    "rt_global_root_register(",
    "rt_global_root_unregister(",
    "rt_gc_root_get(",
    "rt_gc_root_set(",
    "rt_gc_root_set_h(",
    "rt_handle_alloc(",
    "rt_handle_alloc_h(",
    "rt_handle_free(",
    "rt_handle_load(",
    "rt_handle_store(",
    "rt_handle_store_h(",
    "rt_weak_add(",
    "rt_weak_add_h(",
    "rt_weak_get(",
    "rt_weak_remove(",
    "rt_queue_microtask_handle(",
    "rt_queue_microtask_handle_with_drop(",
    "rt_set_timeout_handle(",
    "rt_set_timeout_handle_with_drop(",
    "rt_set_interval_handle(",
    "rt_set_interval_handle_with_drop(",
    "rt_io_register_handle(",
    "rt_io_register_handle_with_drop(",
    "rt_root_push(",
    "rt_root_pop(",
    // Stable native promise + coroutine ABI.
    "rt_promise_init(",
    "rt_promise_fulfill(",
    "rt_promise_try_fulfill(",
    "rt_promise_reject(",
    "rt_promise_try_reject(",
    "rt_promise_mark_handled(",
    "rt_promise_payload_ptr(",
    "rt_async_spawn(",
    "rt_async_spawn_deferred(",
    "rt_async_cancel_all(",
    "rt_async_poll(",
    "rt_async_wait(",
    "rt_async_set_strict_await_yields(",
    "rt_async_run_until_idle(",
    "rt_async_block_on(",
    "rt_gc_set_config(",
    "rt_gc_set_limits(",
    "rt_gc_get_config(",
    "rt_gc_get_limits(",
    "rt_gc_set_young_range(",
    "rt_gc_get_young_range(",
    "rt_gc_poll(",
    "rt_thread_init(",
    "rt_thread_deinit(",
    "rt_thread_register(",
    "rt_thread_unregister(",
    "rt_register_current_thread(",
    "rt_unregister_current_thread(",
    "rt_register_thread(",
    "rt_unregister_thread(",
    "rt_thread_current(",
    "rt_thread_attach(",
    "rt_thread_detach(",
    "rt_keep_alive_gc_ref(",
    "rt_register_shape_table(",
    "rt_register_shape_table_append(",
    "rt_register_shape_table_extend(",
    "rt_register_shape(",
    "rt_parallel_spawn_rooted(",
    "rt_parallel_spawn_rooted_h(",
    // Strings.
    "rt_string_concat(",
    "rt_string_free(",
    "rt_parallel_for_rooted(",
    "rt_parallel_for_rooted_h(",
    "rt_parallel_spawn_promise_rooted(",
    "rt_parallel_spawn_promise_rooted_h(",
    "rt_parallel_spawn_promise_with_shape(",
    "rt_parallel_spawn_promise_with_shape_rooted(",
    "rt_parallel_spawn_promise_with_shape_rooted_h(",
    "rt_queue_microtask_rooted(",
    "rt_async_sleep(",
    "rt_queue_microtask_rooted_h(",
    "rt_queue_microtask(",
    "rt_drain_microtasks(",
    "rt_set_timeout_rooted(",
    "rt_set_timeout_rooted_h(",
    "rt_set_interval_rooted(",
    "rt_set_interval_rooted_h(",
    // I/O watchers.
    "rt_io_register(",
    "rt_io_register_rooted(",
    "rt_io_register_rooted_h(",
    "rt_io_register_with_drop(",
    "rt_io_update(",
     "rt_io_unregister(",
      // Interned strings.
      "rt_string_intern(",
      "rt_string_pin_interned(",
      "rt_string_lookup(",
      "rt_string_lookup_pinned(",
      // Legacy promise resolution ABI.
      "rt_promise_resolve_into_legacy(",
      "rt_promise_resolve_promise_legacy(",
      "rt_promise_resolve_thenable_legacy(",
      "rt_coro_await_value_legacy(",
  ] {
    assert!(
      HEADER.contains(sym),
      "`runtime_native.h` is missing expected ABI symbol: {sym}"
    );
  }

  // Native async ABI versioning.
  let expected = format!(
    "RT_ASYNC_ABI_VERSION = {}",
    runtime_native::async_abi::RT_ASYNC_ABI_VERSION
  );
  assert!(
    HEADER.contains(&expected),
    "`runtime_native.h` is missing/incorrect async ABI version tag (expected to contain `{expected}`)"
  );

  // Array ABI (rt_alloc_array).
  for sym in ["RT_ARRAY_ELEM_PTR_FLAG", "RT_ARRAY_DATA_OFFSET", "typedef struct RtArrayHeader"] {
    assert!(
      HEADER.contains(sym),
      "`runtime_native.h` is missing expected array ABI declaration: {sym}"
    );
  }
  for sym in ["typedef struct RtGcConfig", "typedef struct RtGcLimits"] {
    assert!(
      HEADER.contains(sym),
      "`runtime_native.h` is missing expected GC config ABI declaration: {sym}"
    );
  }
  for field in [
    "const void* type_desc",
    "size_t meta",
    "size_t len",
    "uint32_t elem_size",
    "uint32_t elem_flags",
    "uint8_t data[]",
  ] {
    assert!(
      HEADER.contains(field),
      "`runtime_native.h` is missing expected RtArrayHeader field: {field}"
    );
  }
  assert!(
    HEADER.contains("typedef struct Microtask"),
    "`runtime_native.h` is missing the Microtask ABI type"
  );

  // Parallel → Promise bridge.
  for sym in ["rt_parallel_spawn_promise_legacy("] {
    assert!(
      HEADER.contains(sym),
      "`runtime_native.h` is missing expected ABI symbol: {sym}"
    );
  }

  // Stats APIs are feature-gated on the Rust side; the C header uses a macro
  // guard to avoid exposing unavailable symbols by default.
  assert!(
    HEADER.contains("#ifdef RUNTIME_NATIVE_GC_STATS"),
    "`runtime_native.h` is missing the RUNTIME_NATIVE_GC_STATS feature guard"
  );
  if cfg!(feature = "gc_stats") {
    for sym in ["rt_gc_stats_snapshot(", "rt_gc_stats_reset("] {
      assert!(
        HEADER.contains(sym),
        "`runtime_native.h` is missing expected GC stats ABI symbol: {sym}"
      );
    }
  }

  assert!(
    HEADER.contains("#ifdef RUNTIME_NATIVE_GC_DEBUG"),
    "`runtime_native.h` is missing the RUNTIME_NATIVE_GC_DEBUG feature guard"
  );
  if cfg!(feature = "gc_debug") {
    for sym in ["rt_debug_shape_count(", "rt_debug_shape_descriptor(", "rt_debug_validate_heap("] {
      assert!(
        HEADER.contains(sym),
        "`runtime_native.h` is missing expected GC debug ABI symbol: {sym}"
      );
    }
  }
}

#[test]
fn runtime_native_docs_mention_rooted_parallel_apis() {
  const README: &str = include_str!("../README.md");
  const ASYNC_ABI: &str = include_str!("../docs/async_abi.md");

  for sym in [
    "rt_parallel_spawn_rooted",
    "rt_parallel_spawn_rooted_h",
    "rt_parallel_for_rooted",
    "rt_parallel_for_rooted_h",
    "rt_parallel_spawn_promise_rooted",
    "rt_parallel_spawn_promise_rooted_h",
  ] {
    assert!(
      README.contains(sym),
      "`runtime-native/README.md` is missing mention of rooted parallel API: {sym}"
    );
  }

  for sym in ["rt_parallel_spawn_promise_rooted", "rt_parallel_spawn_promise_rooted_h"] {
    assert!(
      ASYNC_ABI.contains(sym),
      "`runtime-native/docs/async_abi.md` is missing mention of rooted parallel promise API: {sym}"
    );
  }
}

#[test]
fn runtime_native_exports_match_expected_abi_signatures() {
  let _rt = TestRuntimeGuard::new();
  // KeepAlive is an exported C ABI symbol but not part of the Rust public API surface, so we bind
  // it via an extern declaration here to ensure the signature stays in sync with the header.
  extern "C" {
    fn rt_keep_alive_gc_ref(gc_ref: *mut u8);
  }

  // Thread registration.
  let _thread_init: extern "C" fn(u32) = runtime_native::rt_thread_init;
  let _thread_deinit: extern "C" fn() = runtime_native::rt_thread_deinit;
  let _thread_register: extern "C" fn(runtime_native::abi::RtThreadKind) -> u64 = runtime_native::rt_thread_register;
  let _thread_unregister: extern "C" fn() = runtime_native::rt_thread_unregister;
  let _register_current: extern "C" fn() = runtime_native::rt_register_current_thread;
  let _unregister_current: extern "C" fn() = runtime_native::rt_unregister_current_thread;
  let _register_thread: extern "C" fn() = runtime_native::rt_register_thread;
  let _unregister_thread: extern "C" fn() = runtime_native::rt_unregister_thread;
  let _thread_current: extern "C" fn() -> *mut runtime_native::Thread = runtime_native::rt_thread_current;
  let _thread_attach: unsafe extern "C" fn(*mut runtime_native::Runtime) -> *mut runtime_native::Thread =
    runtime_native::rt_thread_attach;
  let _thread_detach: unsafe extern "C" fn(*mut runtime_native::Thread) = runtime_native::rt_thread_detach;

  let _stackmaps_register: extern "C" fn(*const u8, *const u8) -> bool =
    runtime_native::rt_stackmaps_register;
  let _stackmaps_unregister: extern "C" fn(*const u8) -> bool =
    runtime_native::rt_stackmaps_unregister;

  // GC write barrier entrypoints.
  let _gc_poll: extern "C" fn() -> bool = runtime_native::rt_gc_poll;
  let _write_barrier: unsafe extern "C" fn(*mut u8, *mut u8) = runtime_native::rt_write_barrier;
  let _write_barrier_range: unsafe extern "C" fn(*mut u8, *mut u8, usize) =
    runtime_native::rt_write_barrier_range;
  let _backing_store_external_bytes: extern "C" fn() -> usize =
    runtime_native::rt_backing_store_external_bytes;
  let _keep_alive: unsafe extern "C" fn(*mut u8) = rt_keep_alive_gc_ref;

  // Strings.
  let _string_concat: extern "C" fn(*const u8, usize, *const u8, usize) -> runtime_native::abi::StringRef =
    runtime_native::rt_string_concat;
  let _string_intern: extern "C" fn(*const u8, usize) -> runtime_native::abi::InternedId =
    runtime_native::rt_string_intern;
  let _string_pin_interned: extern "C" fn(runtime_native::abi::InternedId) =
    runtime_native::rt_string_pin_interned;
  let _string_lookup: extern "C" fn(runtime_native::abi::InternedId) -> runtime_native::abi::StringRef =
    runtime_native::rt_string_lookup;
  let _string_lookup_pinned: unsafe extern "C" fn(
    runtime_native::abi::InternedId,
    *mut runtime_native::abi::StringRef,
  ) -> bool = runtime_native::rt_string_lookup_pinned;

  // Parallel → Promise bridge.
  let _parallel_spawn_promise: extern "C" fn(
    extern "C" fn(*mut u8, runtime_native::abi::PromiseRef),
    *mut u8,
  ) -> runtime_native::abi::PromiseRef = runtime_native::rt_parallel_spawn_promise_legacy;

  // Stable native promise + coroutine ABI.
  let _promise_init: unsafe extern "C" fn(runtime_native::PromiseRef) = runtime_native::rt_promise_init;
  let _promise_fulfill: unsafe extern "C" fn(runtime_native::PromiseRef) = runtime_native::rt_promise_fulfill;
  let _promise_try_fulfill: unsafe extern "C" fn(runtime_native::PromiseRef) -> bool = runtime_native::rt_promise_try_fulfill;
  let _promise_reject: unsafe extern "C" fn(runtime_native::PromiseRef) = runtime_native::rt_promise_reject;
  let _promise_try_reject: unsafe extern "C" fn(runtime_native::PromiseRef) -> bool = runtime_native::rt_promise_try_reject;
  let _promise_mark_handled: unsafe extern "C" fn(runtime_native::PromiseRef) = runtime_native::rt_promise_mark_handled;
  let _promise_payload_ptr: extern "C" fn(runtime_native::PromiseRef) -> *mut u8 =
    runtime_native::rt_promise_payload_ptr;
  let _async_spawn: unsafe extern "C" fn(runtime_native::CoroutineId) -> runtime_native::PromiseRef =
    runtime_native::rt_async_spawn;
  let _async_spawn_deferred: unsafe extern "C" fn(runtime_native::CoroutineId) -> runtime_native::PromiseRef =
    runtime_native::rt_async_spawn_deferred;
  let _async_cancel_all: extern "C" fn() = runtime_native::rt_async_cancel_all;
  let _async_poll: extern "C" fn() -> bool = runtime_native::rt_async_poll;
  let _async_wait: extern "C" fn() = runtime_native::rt_async_wait;
  let _async_set_strict_await_yields: extern "C" fn(bool) = runtime_native::rt_async_set_strict_await_yields;
  let _async_run_until_idle: unsafe extern "C" fn() -> bool = runtime_native::rt_async_run_until_idle_abi;
  let _async_block_on: unsafe extern "C" fn(runtime_native::PromiseRef) = runtime_native::rt_async_block_on;

  // Global root registration.
  let _register_root_slot: extern "C" fn(*mut *mut u8) -> u32 =
    runtime_native::rt_gc_register_root_slot;
  let _unregister_root_slot: extern "C" fn(u32) = runtime_native::rt_gc_unregister_root_slot;
  let _pin: extern "C" fn(*mut u8) -> u32 = runtime_native::rt_gc_pin;
  let _pin_h: unsafe extern "C" fn(runtime_native::roots::GcHandle) -> u32 = runtime_native::rt_gc_pin_h;
  let _unpin: extern "C" fn(u32) = runtime_native::rt_gc_unpin;
  let _global_root_register: extern "C" fn(*mut usize) = runtime_native::rt_global_root_register;
  let _global_root_unregister: extern "C" fn(*mut usize) = runtime_native::rt_global_root_unregister;
  let _root_get: extern "C" fn(u32) -> *mut u8 = runtime_native::rt_gc_root_get;
  let _root_set: extern "C" fn(u32, *mut u8) -> bool = runtime_native::rt_gc_root_set;
  let _root_set_h: unsafe extern "C" fn(u32, runtime_native::roots::GcHandle) -> bool =
    runtime_native::rt_gc_root_set_h;

  // Persistent handles (stable u64 IDs).
  let _handle_alloc: extern "C" fn(*mut u8) -> u64 = runtime_native::rt_handle_alloc;
  let _handle_alloc_h: unsafe extern "C" fn(runtime_native::roots::GcHandle) -> u64 =
    runtime_native::rt_handle_alloc_h;
  let _handle_free: extern "C" fn(u64) = runtime_native::rt_handle_free;
  let _handle_load: extern "C" fn(u64) -> *mut u8 = runtime_native::rt_handle_load;
  let _handle_store: extern "C" fn(u64, *mut u8) = runtime_native::rt_handle_store;
  let _handle_store_h: unsafe extern "C" fn(u64, runtime_native::roots::GcHandle) =
    runtime_native::rt_handle_store_h;
  let _weak_add_h: unsafe extern "C" fn(runtime_native::roots::GcHandle) -> u64 = runtime_native::rt_weak_add_h;

  // Microtasks.
  let _queue_microtask: unsafe extern "C" fn(runtime_native::abi::Microtask) =
    runtime_native::rt_queue_microtask;
  let _queue_microtask_with_drop: extern "C" fn(extern "C" fn(*mut u8), *mut u8, extern "C" fn(*mut u8)) =
    runtime_native::rt_queue_microtask_with_drop;
  let _queue_microtask_handle: extern "C" fn(extern "C" fn(*mut u8), u64) =
    runtime_native::rt_queue_microtask_handle;
  let _queue_microtask_handle_with_drop: extern "C" fn(extern "C" fn(*mut u8), u64, extern "C" fn(*mut u8)) =
    runtime_native::rt_queue_microtask_handle_with_drop;
  let _drain_microtasks: extern "C" fn() -> bool = runtime_native::rt_drain_microtasks_abi;

  // Strings.
  let _string_concat: extern "C" fn(*const u8, usize, *const u8, usize) -> runtime_native::StringRef =
    runtime_native::rt_string_concat;
  let _string_free: extern "C" fn(runtime_native::StringRef) = runtime_native::rt_string_free;

  // Per-thread shadow stack root push/pop.
  let _root_push: unsafe extern "C" fn(runtime_native::roots::GcHandle) = runtime_native::rt_root_push;
  let _root_pop: unsafe extern "C" fn(runtime_native::roots::GcHandle) = runtime_native::rt_root_pop;
  // Rooted scheduling entrypoints.
  let _parallel_spawn_rooted: extern "C" fn(extern "C" fn(*mut u8), *mut u8) -> runtime_native::abi::TaskId =
    runtime_native::rt_parallel_spawn_rooted;
  let _parallel_spawn_rooted_h: unsafe extern "C" fn(extern "C" fn(*mut u8), runtime_native::roots::GcHandle) -> runtime_native::abi::TaskId =
    runtime_native::rt_parallel_spawn_rooted_h;
  let _parallel_for: extern "C" fn(
    usize,
    usize,
    runtime_native::abi::RtParallelForBodyFn,
    *mut u8,
  ) = runtime_native::rt_parallel_for;
  let _parallel_for_rooted: extern "C" fn(
    usize,
    usize,
    runtime_native::abi::RtParallelForBodyFn,
    *mut u8,
  ) = runtime_native::rt_parallel_for_rooted;
  let _parallel_for_rooted_h: unsafe extern "C" fn(
    usize,
    usize,
    runtime_native::abi::RtParallelForBodyFn,
    runtime_native::roots::GcHandle,
  ) = runtime_native::rt_parallel_for_rooted_h;
  let _queue_microtask_rooted: extern "C" fn(extern "C" fn(*mut u8), *mut u8) = runtime_native::rt_queue_microtask_rooted;
  let _parallel_spawn_promise_rooted: extern "C" fn(
    extern "C" fn(*mut u8, runtime_native::abi::PromiseRef),
    *mut u8,
    runtime_native::PromiseLayout,
  ) -> runtime_native::abi::PromiseRef = runtime_native::rt_parallel_spawn_promise_rooted;
  let _parallel_spawn_promise_rooted_h: unsafe extern "C" fn(
    extern "C" fn(*mut u8, runtime_native::abi::PromiseRef),
    runtime_native::roots::GcHandle,
    runtime_native::PromiseLayout,
  ) -> runtime_native::abi::PromiseRef = runtime_native::rt_parallel_spawn_promise_rooted_h;
  let _queue_microtask_rooted_h: unsafe extern "C" fn(extern "C" fn(*mut u8), runtime_native::roots::GcHandle) =
    runtime_native::rt_queue_microtask_rooted_h;
  let _set_timeout_rooted: extern "C" fn(extern "C" fn(*mut u8), *mut u8, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_timeout_rooted;
  let _set_timeout_rooted_h: unsafe extern "C" fn(extern "C" fn(*mut u8), runtime_native::roots::GcHandle, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_timeout_rooted_h;
  let _set_timeout_handle: extern "C" fn(extern "C" fn(*mut u8), u64, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_timeout_handle;
  let _set_timeout_handle_with_drop: extern "C" fn(
    extern "C" fn(*mut u8),
    u64,
    extern "C" fn(*mut u8),
    u64,
  ) -> runtime_native::abi::TimerId = runtime_native::rt_set_timeout_handle_with_drop;
  let _set_interval_rooted: extern "C" fn(extern "C" fn(*mut u8), *mut u8, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_interval_rooted;
  let _set_interval_rooted_h: unsafe extern "C" fn(extern "C" fn(*mut u8), runtime_native::roots::GcHandle, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_interval_rooted_h;
  let _set_interval_handle: extern "C" fn(extern "C" fn(*mut u8), u64, u64) -> runtime_native::abi::TimerId =
    runtime_native::rt_set_interval_handle;
  let _set_interval_handle_with_drop: extern "C" fn(
    extern "C" fn(*mut u8),
    u64,
    extern "C" fn(*mut u8),
    u64,
  ) -> runtime_native::abi::TimerId = runtime_native::rt_set_interval_handle_with_drop;
  // I/O watchers.
  let _io_register: extern "C" fn(
    i32,
    u32,
    extern "C" fn(u32, *mut u8),
    *mut u8,
  ) -> runtime_native::abi::IoWatcherId = runtime_native::rt_io_register;
  let _io_register_with_drop: extern "C" fn(
    i32,
    u32,
    extern "C" fn(u32, *mut u8),
    *mut u8,
    extern "C" fn(*mut u8),
  ) -> runtime_native::abi::IoWatcherId = runtime_native::rt_io_register_with_drop;
  let _io_register_rooted: extern "C" fn(i32, u32, extern "C" fn(u32, *mut u8), *mut u8) -> runtime_native::abi::IoWatcherId =
    runtime_native::rt_io_register_rooted;
  let _io_register_rooted_h: unsafe extern "C" fn(i32, u32, extern "C" fn(u32, *mut u8), runtime_native::roots::GcHandle) -> runtime_native::abi::IoWatcherId =
    runtime_native::rt_io_register_rooted_h;
  let _io_register_handle: extern "C" fn(
    i32,
    u32,
    extern "C" fn(u32, *mut u8),
    u64,
  ) -> runtime_native::abi::IoWatcherId = runtime_native::rt_io_register_handle;
  let _io_register_handle_with_drop: extern "C" fn(
    i32,
    u32,
    extern "C" fn(u32, *mut u8),
    u64,
    extern "C" fn(*mut u8),
  ) -> runtime_native::abi::IoWatcherId = runtime_native::rt_io_register_handle_with_drop;
  let _io_update: extern "C" fn(runtime_native::abi::IoWatcherId, u32) = runtime_native::rt_io_update;
  let _io_unregister: extern "C" fn(runtime_native::abi::IoWatcherId) = runtime_native::rt_io_unregister;

  // Promise resolution helpers (legacy promises).
  let _promise_resolve_into_legacy: extern "C" fn(runtime_native::abi::PromiseRef, runtime_native::abi::PromiseResolveInput) =
    runtime_native::rt_promise_resolve_into_legacy;
  let _promise_resolve_promise_legacy: extern "C" fn(runtime_native::abi::PromiseRef, runtime_native::abi::PromiseRef) =
    runtime_native::rt_promise_resolve_promise_legacy;
  let _promise_resolve_thenable_legacy: extern "C" fn(runtime_native::abi::PromiseRef, runtime_native::abi::ThenableRef) =
    runtime_native::rt_promise_resolve_thenable_legacy;
  let _coro_await_value_legacy: extern "C" fn(
    *mut runtime_native::abi::RtCoroutineHeader,
    runtime_native::abi::PromiseResolveInput,
    u32,
  ) = runtime_native::rt_coro_await_value_legacy;

  #[cfg(feature = "gc_stats")]
  {
    let _stats_snapshot: unsafe extern "C" fn(*mut runtime_native::abi::RtGcStatsSnapshot) =
      runtime_native::rt_gc_stats_snapshot;
    let _stats_reset: extern "C" fn() = runtime_native::rt_gc_stats_reset;
    let _ = (_stats_snapshot, _stats_reset);
  }

  #[cfg(feature = "gc_debug")]
  {
    extern "C" {
      fn rt_debug_shape_count() -> usize;
      fn rt_debug_shape_descriptor(
        id: runtime_native::abi::RtShapeId,
      ) -> *const runtime_native::abi::RtShapeDescriptor;
      fn rt_debug_validate_heap();
    }

    let _debug_shape_count: unsafe extern "C" fn() -> usize = rt_debug_shape_count;
    let _debug_shape_descriptor: unsafe extern "C" fn(
      runtime_native::abi::RtShapeId,
    ) -> *const runtime_native::abi::RtShapeDescriptor = rt_debug_shape_descriptor;
    let _debug_validate_heap: unsafe extern "C" fn() = rt_debug_validate_heap;
    let _ = (_debug_shape_count, _debug_shape_descriptor, _debug_validate_heap);
  }

  let _ = (
    _thread_init,
    _thread_deinit,
    _thread_register,
    _thread_unregister,
    _register_current,
    _unregister_current,
    _stackmaps_register,
    _stackmaps_unregister,
    _gc_poll,
    _register_thread,
    _unregister_thread,
    _thread_attach,
    _thread_detach,
    _write_barrier,
    _write_barrier_range,
    _backing_store_external_bytes,
    _keep_alive,
    _string_concat,
    _string_intern,
    _string_pin_interned,
    _string_lookup,
    _string_lookup_pinned,
    _parallel_spawn_promise,
    _promise_init,
    _promise_fulfill,
    _promise_try_fulfill,
    _promise_reject,
    _promise_try_reject,
    _promise_mark_handled,
    _promise_payload_ptr,
    _async_spawn,
    _async_spawn_deferred,
    _async_cancel_all,
    _async_poll,
    _async_wait,
    _async_set_strict_await_yields,
    _async_run_until_idle,
    _async_block_on,
    _register_root_slot,
    _unregister_root_slot,
    _pin,
    _pin_h,
    _unpin,
    _global_root_register,
    _global_root_unregister,
    _root_get,
    _root_set,
    _root_set_h,
    _handle_alloc,
    _handle_alloc_h,
    _handle_free,
    _handle_load,
    _handle_store,
    _handle_store_h,
    _weak_add_h,
    _queue_microtask,
    _queue_microtask_with_drop,
    _queue_microtask_handle,
    _queue_microtask_handle_with_drop,
    _drain_microtasks,
    _string_concat,
    _string_free,
    _root_push,
    _root_pop,
    _parallel_spawn_rooted,
    _parallel_spawn_rooted_h,
    _parallel_for,
    _parallel_for_rooted,
    _queue_microtask_rooted,
    _queue_microtask_rooted_h,
    _parallel_spawn_promise_rooted,
    _parallel_spawn_promise_rooted_h,
    _set_timeout_rooted,
    _set_timeout_rooted_h,
    _set_timeout_handle,
    _set_timeout_handle_with_drop,
    _set_interval_rooted,
    _set_interval_rooted_h,
    _set_interval_handle,
    _set_interval_handle_with_drop,
    _io_register,
    _io_register_with_drop,
    _io_register_rooted,
    _io_register_rooted_h,
    _io_register_handle,
    _io_register_handle_with_drop,
    _io_update,
    _io_unregister,
    _promise_resolve_into_legacy,
    _promise_resolve_promise_legacy,
    _promise_resolve_thenable_legacy,
    _coro_await_value_legacy,
  );
}

fn workspace_root() -> PathBuf {
  // runtime-native/ is a workspace member; workspace root is its parent.
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("runtime-native should live at <workspace>/runtime-native")
    .to_path_buf()
}

fn target_dir() -> PathBuf {
  std::env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root().join("target"))
}

fn find_staticlib(target_dir: &Path, profile: &str) -> PathBuf {
  let direct = target_dir.join(profile).join("libruntime_native.a");
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = fs::metadata(&direct)
    .and_then(|meta| meta.modified())
    .ok()
    .map(|mtime| (mtime, direct.clone()));

  let deps_dir = target_dir.join(profile).join("deps");
  if let Ok(entries) = fs::read_dir(&deps_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        continue;
      };
      if !(file_name.starts_with("libruntime_native") && file_name.ends_with(".a")) {
        continue;
      }

      let mtime = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      match newest {
        Some((best, _)) if mtime <= best => {}
        _ => newest = Some((mtime, path)),
      }
    }
  }

  if let Some((_, path)) = newest {
    return path;
  }

  panic!(
    "failed to find runtime-native staticlib at {} (checked {} and {})",
    target_dir.display(),
    direct.display(),
    deps_dir.display()
  );
}

#[test]
fn runtime_native_c_header_declares_all_exported_rt_symbols() {
  let _rt = TestRuntimeGuard::new();
  // This repo's CI target is Ubuntu x86_64. Use GNU nm to sanity check that
  // every exported `rt_*` entrypoint in the staticlib is declared in the C
  // header.
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: header/export sync via `nm` is only checked on Linux");
    return;
  }

  // `nm` is part of the standard toolchain on Ubuntu; still skip gracefully if absent.
  if !Command::new("nm")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
  {
    eprintln!("skipping: `nm` not available");
    return;
  }

  let header = include_str!("../include/runtime_native.h");
  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let staticlib = find_staticlib(&target_dir(), profile);

  let out = Command::new("nm")
    .arg("-g")
    .arg("--defined-only")
    .arg(&staticlib)
    .output()
    .expect("run nm");
  assert!(
    out.status.success(),
    "nm failed for {}: status={:?} stderr={}",
    staticlib.display(),
    out.status,
    String::from_utf8_lossy(&out.stderr)
  );

  let stdout = String::from_utf8_lossy(&out.stdout);
  let mut missing: Vec<String> = Vec::new();
  for line in stdout.lines() {
    let line = line.trim();
    if line.is_empty() || line.ends_with(':') {
      continue;
    }
    let Some(name) = line.split_whitespace().last() else {
      continue;
    };
    if !name.starts_with("rt_") {
      continue;
    }
    let needle = format!("{name}(");
    if !header.contains(&needle) {
      missing.push(name.to_string());
    }
  }

  if !missing.is_empty() {
    missing.sort();
    missing.dedup();
    panic!(
      "runtime_native.h is missing declarations for exported rt_* symbols from {}:\n{}",
      staticlib.display(),
      missing.join("\n")
    );
  }
}

#[test]
fn runtime_native_staticlib_exports_expected_global_symbols() {
  let _rt = TestRuntimeGuard::new();
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: global symbol export check is only supported on Linux");
    return;
  }

  // These symbols are part of the stable codegen/runtime ABI, but they're not `rt_*` functions:
  // - `RT_GC_EPOCH`: safepoint epoch polled directly by generated code.
  // - `RT_THREAD`:   TLS pointer to the current per-thread runtime record.
  const EXPECTED_GLOBALS: &[&str] = &["RT_GC_EPOCH", "RT_THREAD"];

  // `nm` is part of the standard toolchain on Ubuntu; still skip gracefully if absent.
  if !Command::new("nm")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
  {
    eprintln!("skipping: `nm` not available");
    return;
  }

  let header = include_str!("../include/runtime_native.h");
  for sym in EXPECTED_GLOBALS {
    assert!(
      header.contains(sym),
      "`runtime_native.h` is missing expected exported global symbol declaration: {sym}"
    );
  }

  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let staticlib = find_staticlib(&target_dir(), profile);

  let out = Command::new("nm")
    .arg("-g")
    .arg("--defined-only")
    .arg(&staticlib)
    .output()
    .expect("run nm");
  assert!(
    out.status.success(),
    "nm failed for {}: status={:?} stderr={}",
    staticlib.display(),
    out.status,
    String::from_utf8_lossy(&out.stderr)
  );
  let stdout = String::from_utf8_lossy(&out.stdout);
  let mut exported: std::collections::HashSet<&str> = std::collections::HashSet::new();
  for line in stdout.lines() {
    let line = line.trim();
    if line.is_empty() || line.ends_with(':') {
      continue;
    }
    if let Some(name) = line.split_whitespace().last() {
      exported.insert(name);
    }
  }
  for &sym in EXPECTED_GLOBALS {
    assert!(
      exported.contains(sym),
      "expected {sym} to be defined in {}, but it was not present in `nm -g --defined-only` output",
      staticlib.display()
    );
  }

  // Best-effort: verify that RT_THREAD is actually emitted as a TLS symbol (not a plain global).
  // This requires `readelf` (binutils); skip if unavailable.
  if !Command::new("readelf")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
  {
    eprintln!("skipping: `readelf` not available; cannot verify RT_THREAD TLS symbol type");
    return;
  }

  let out = Command::new("readelf")
    .arg("-s")
    .arg(&staticlib)
    .output()
    .expect("run readelf");
  assert!(
    out.status.success(),
    "readelf failed for {}: status={:?} stderr={}",
    staticlib.display(),
    out.status,
    String::from_utf8_lossy(&out.stderr)
  );
  let stdout = String::from_utf8_lossy(&out.stdout);
  let mut saw_tls_rt_thread = false;
  let mut saw_defined_gc_epoch = false;
  for line in stdout.lines() {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 8 {
      continue;
    }
    let name = tokens[tokens.len() - 1];
    if name == "RT_THREAD" {
      // readelf columns (typical):
      //   Num: Value Size Type Bind Vis Ndx Name
      // We want: Type=TLS and Ndx != UND.
      let ty = tokens.get(3).copied().unwrap_or_default();
      let ndx = tokens.get(6).copied().unwrap_or_default();
      if ty == "TLS" && ndx != "UND" {
        saw_tls_rt_thread = true;
      }
    } else if name == "RT_GC_EPOCH" {
      let ndx = tokens.get(6).copied().unwrap_or_default();
      if ndx != "UND" {
        saw_defined_gc_epoch = true;
      }
    }
  }
  assert!(
    saw_tls_rt_thread,
    "expected RT_THREAD to be a defined TLS symbol in {} (via `readelf -s`), but it was not found",
    staticlib.display()
  );
  assert!(
    saw_defined_gc_epoch,
    "expected RT_GC_EPOCH to be defined in {} (via `readelf -s`), but it was not found",
    staticlib.display()
  );
}

fn find_cdylib(target_dir: &Path, profile: &str) -> PathBuf {
  let direct = target_dir.join(profile).join("libruntime_native.so");
  let deps_dir = target_dir.join(profile).join("deps");
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

  if direct.is_file() {
    let mtime = fs::metadata(&direct)
      .and_then(|meta| meta.modified())
      .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    newest = Some((mtime, direct.clone()));
  }

  if let Ok(entries) = fs::read_dir(&deps_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        continue;
      };
      if !(file_name.starts_with("libruntime_native") && file_name.ends_with(".so")) {
        continue;
      }
      let mtime = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      match newest {
        Some((best, _)) if mtime <= best => {}
        _ => newest = Some((mtime, path)),
      }
    }
  }

  if let Some((_, path)) = newest {
    return path;
  }

  panic!(
    "failed to find runtime-native cdylib at {} (checked {} and {})",
    target_dir.display(),
    direct.display(),
    deps_dir.display()
  );
}

#[test]
fn runtime_native_cdylib_exports_rt_symbols() {
  let _rt = TestRuntimeGuard::new();
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: cdylib export check is only supported on Linux");
    return;
  }

  // `nm` is part of the standard toolchain on Ubuntu; still skip gracefully if absent.
  if Command::new("nm")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_err()
  {
    eprintln!("skipping: `nm` not available");
    return;
  }

  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let staticlib = find_staticlib(&target_dir(), profile);
  let cdylib = find_cdylib(&target_dir(), profile);

  // Extract the set of `rt_*` symbols defined by the archive and compare it to the set exported from
  // the shared library. This prevents regressions where an ABI entrypoint is implemented in
  // `global_asm!` (or other non-Rust objects) and is therefore omitted from `cdylib` exports by Rust's
  // version-script-based export filtering.
  let static_out = Command::new("nm")
    .arg("-g")
    .arg("--defined-only")
    .arg(&staticlib)
    .output()
    .expect("run nm on staticlib");
  assert!(
    static_out.status.success(),
    "nm failed for {}: status={:?} stderr={}",
    staticlib.display(),
    static_out.status,
    String::from_utf8_lossy(&static_out.stderr)
  );

  let dylib_out = Command::new("nm")
    .arg("-D")
    .arg("--defined-only")
    .arg(&cdylib)
    .output()
    .expect("run nm on cdylib");
  assert!(
    dylib_out.status.success(),
    "nm failed for {}: status={:?} stderr={}",
    cdylib.display(),
    dylib_out.status,
    String::from_utf8_lossy(&dylib_out.stderr)
  );

  let static_stdout = String::from_utf8_lossy(&static_out.stdout);
  let dylib_stdout = String::from_utf8_lossy(&dylib_out.stdout);

  fn strip_symbol_version(name: &str) -> &str {
    name.split_once('@').map(|(base, _)| base).unwrap_or(name)
  }

  let static_syms: std::collections::BTreeSet<String> = static_stdout
    .lines()
    .filter_map(|line| line.split_whitespace().last().map(strip_symbol_version))
    .filter(|name| name.starts_with("rt_"))
    .map(|s| s.to_string())
    .collect();
  let dylib_syms: std::collections::BTreeSet<String> = dylib_stdout
    .lines()
    .filter_map(|line| line.split_whitespace().last().map(strip_symbol_version))
    .filter(|name| name.starts_with("rt_"))
    .map(|s| s.to_string())
    .collect();

  let missing: Vec<String> = static_syms.difference(&dylib_syms).cloned().collect();
  let extra: Vec<String> = dylib_syms.difference(&static_syms).cloned().collect();
  if !missing.is_empty() || !extra.is_empty() {
    panic!(
      "runtime-native cdylib exports do not match staticlib:\nmissing in .so:\n{}\nextra in .so:\n{}",
      missing.join("\n"),
      extra.join("\n"),
    );
  }

  // `place-safepoints` inserts calls to a symbol named `gc.safepoint_poll`. Ensure the runtime
  // shared library exports it so native modules can link against `libruntime_native.so` directly.
  assert!(
    dylib_stdout.contains("gc.safepoint_poll"),
    "expected gc.safepoint_poll to be exported from {}",
    cdylib.display()
  );

  // Generated code typically polls the safepoint epoch directly. Ensure the exported global is
  // visible from the shared library too.
  assert!(
    dylib_stdout.lines().any(|line| {
      line
        .split_whitespace()
        .last()
        .is_some_and(|name| strip_symbol_version(name) == "RT_GC_EPOCH")
    }),
    "expected RT_GC_EPOCH to be exported from {}",
    cdylib.display()
  );
}
