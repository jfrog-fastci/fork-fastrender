#[test]
fn runtime_native_c_header_contains_expected_abi_symbols() {
  const HEADER: &str = include_str!("../include/runtime_native.h");

  // Core barrier + GC range plumbing.
  for sym in [
    "rt_write_barrier(",
    "rt_write_barrier_range(",
    "rt_gc_register_root_slot(",
    "rt_gc_unregister_root_slot(",
    "rt_gc_pin(",
    "rt_gc_unpin(",
    "rt_gc_set_young_range(",
    "rt_gc_get_young_range(",
    "rt_thread_init(",
    "rt_thread_deinit(",
  ] {
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
}

#[test]
fn runtime_native_exports_match_expected_abi_signatures() {
  // Thread registration.
  let _thread_init: extern "C" fn(u32) = runtime_native::rt_thread_init;
  let _thread_deinit: extern "C" fn() = runtime_native::rt_thread_deinit;

  // GC write barrier entrypoints.
  let _write_barrier: unsafe extern "C" fn(*mut u8, *mut u8) = runtime_native::rt_write_barrier;
  let _write_barrier_range: unsafe extern "C" fn(*mut u8, *mut u8, usize) =
    runtime_native::rt_write_barrier_range;

  // Global root registration.
  let _register_root_slot: extern "C" fn(*mut *mut u8) -> u32 = runtime_native::rt_gc_register_root_slot;
  let _unregister_root_slot: extern "C" fn(u32) = runtime_native::rt_gc_unregister_root_slot;
  let _pin: extern "C" fn(*mut u8) -> u32 = runtime_native::rt_gc_pin;
  let _unpin: extern "C" fn(u32) = runtime_native::rt_gc_unpin;

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
    _write_barrier,
    _write_barrier_range,
    _register_root_slot,
    _unregister_root_slot,
    _pin,
    _unpin,
  );
}
