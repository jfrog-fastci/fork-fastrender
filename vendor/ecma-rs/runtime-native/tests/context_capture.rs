use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::mpsc;
use std::time::Duration;

extern "C" {
  fn rt_gc_safepoint_slow(epoch: u64);
}

#[inline(never)]
fn enter_safepoint_slow(epoch: u64) {
  // Safety: `rt_gc_safepoint_slow` is a runtime-native entrypoint that follows the C ABI.
  unsafe {
    rt_gc_safepoint_slow(epoch);
  }
}

fn resolve_symbol_name(ip: usize) -> Option<String> {
  let mut out = None;
  backtrace::resolve(ip as *mut std::ffi::c_void, |sym| {
    if let Some(name) = sym.name() {
      out = Some(name.to_string());
    }
  });
  out
}

#[test]
fn captures_mutator_ip_and_stack_pointer() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let (tx_id, rx_id) = mpsc::channel();
  let (tx_epoch, rx_epoch) = mpsc::channel();

  let handle = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    let epoch = rx_epoch.recv().unwrap();
    enter_safepoint_slow(epoch);

    threading::unregister_current_thread();
  });

  let worker_id = rx_id.recv().unwrap();

  let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  tx_epoch.send(stop_epoch).unwrap();

  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2)),
    "worker did not reach safepoint in time"
  );

  let worker = threading::all_threads()
    .into_iter()
    .find(|t| t.id().get() == worker_id)
    .expect("worker thread state");

  let ctx = worker
    .safepoint_context()
    .expect("worker should have published a safepoint context");

  // sp should be at least word-aligned on all supported targets.
  assert_eq!(
    ctx.sp_entry % runtime_native::arch::WORD_SIZE,
    0,
    "sp_entry is not word-aligned"
  );

  let bounds = worker
    .stack_bounds()
    .expect("worker thread should have stack bounds recorded");
  assert!(
    ctx.sp_entry >= bounds.lo && ctx.sp_entry < bounds.hi,
    "sp_entry {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.sp_entry,
    bounds.lo,
    bounds.hi
  );

  // Resolve the recorded return address to a symbol name; it should point back
  // into `enter_safepoint_slow` (the call site that entered the slow path).
  //
  // Use `ip - 1` to bias into the calling instruction for symbol lookup.
  let ip_for_lookup = ctx.ip.saturating_sub(1);
  if let Some(sym) = resolve_symbol_name(ip_for_lookup).filter(|s| !s.is_empty()) {
    assert!(
      sym.contains("enter_safepoint_slow"),
      "expected ip {:#x} to resolve to enter_safepoint_slow, got symbol: {sym:?}",
      ctx.ip
    );
  } else {
    // Release builds for this workspace strip symbols, so backtrace resolution can legitimately
    // fail. Fall back to a coarse range check: the return address should still fall within the
    // helper function's code range.
    let base = enter_safepoint_slow as usize;
    assert!(
      ctx.ip >= base && ctx.ip < base + 0x10_000,
      "expected ip {:#x} to fall within enter_safepoint_slow [{:#x}, {:#x})",
      ctx.ip,
      base,
      base + 0x10_000
    );
  }

  runtime_native::rt_gc_resume_world();

  handle.join().unwrap();
  threading::unregister_current_thread();
}
