#[test]
fn runtime_native_c_header_contains_expected_abi_symbols() {
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
    "rt_gc_unpin(",
    "rt_global_root_register(",
    "rt_global_root_unregister(",
    "rt_root_push(",
    "rt_root_pop(",
    "rt_gc_set_young_range(",
    "rt_gc_get_young_range(",
    "rt_gc_poll(",
    "rt_thread_init(",
    "rt_thread_deinit(",
    "rt_register_current_thread(",
    "rt_unregister_current_thread(",
    "rt_register_thread(",
    "rt_unregister_thread(",
    "rt_thread_attach(",
    "rt_thread_detach(",
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
}

#[test]
fn runtime_native_exports_match_expected_abi_signatures() {
  // Thread registration.
  let _thread_init: extern "C" fn(u32) = runtime_native::rt_thread_init;
  let _thread_deinit: extern "C" fn() = runtime_native::rt_thread_deinit;
  let _register_current: extern "C" fn() = runtime_native::rt_register_current_thread;
  let _unregister_current: extern "C" fn() = runtime_native::rt_unregister_current_thread;
  let _register_thread: extern "C" fn() = runtime_native::rt_register_thread;
  let _unregister_thread: extern "C" fn() = runtime_native::rt_unregister_thread;
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

  // Global root registration.
  let _register_root_slot: extern "C" fn(*mut *mut u8) -> u32 =
    runtime_native::rt_gc_register_root_slot;
  let _unregister_root_slot: extern "C" fn(u32) = runtime_native::rt_gc_unregister_root_slot;
  let _pin: extern "C" fn(*mut u8) -> u32 = runtime_native::rt_gc_pin;
  let _unpin: extern "C" fn(u32) = runtime_native::rt_gc_unpin;
  let _global_root_register: extern "C" fn(*mut usize) = runtime_native::rt_global_root_register;
  let _global_root_unregister: extern "C" fn(*mut usize) = runtime_native::rt_global_root_unregister;

  // Per-thread shadow stack root push/pop.
  let _root_push: unsafe extern "C" fn(runtime_native::roots::GcHandle) = runtime_native::rt_root_push;
  let _root_pop: unsafe extern "C" fn(runtime_native::roots::GcHandle) = runtime_native::rt_root_pop;

  #[cfg(feature = "gc_stats")]
  {
    let _stats_snapshot: unsafe extern "C" fn(*mut runtime_native::abi::RtGcStatsSnapshot) =
      runtime_native::rt_gc_stats_snapshot;
    let _stats_reset: extern "C" fn() = runtime_native::rt_gc_stats_reset;
    let _ = (_stats_snapshot, _stats_reset);
  }

  let _ = (
    _thread_init,
    _thread_deinit,
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
    _register_root_slot,
    _unregister_root_slot,
    _pin,
    _unpin,
    _global_root_register,
    _global_root_unregister,
    _root_push,
    _root_pop,
  );
}
