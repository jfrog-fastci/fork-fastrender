use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::Duration;

struct JoinOnDrop(Option<std::thread::JoinHandle<()>>);

impl JoinOnDrop {
  fn new(handle: std::thread::JoinHandle<()>) -> Self {
    Self(Some(handle))
  }

  fn join(&mut self) {
    if let Some(handle) = self.0.take() {
      // If the worker thread panicked, propagate as a test failure.
      handle.join().unwrap();
    }
  }
}

impl Drop for JoinOnDrop {
  fn drop(&mut self) {
    if let Some(handle) = self.0.take() {
      // Don't panic in Drop during unwinding; best-effort join to avoid leaving background threads
      // running between tests.
      let _ = handle.join();
    }
  }
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    // Safe to call even if the world is already resumed.
    runtime_native::rt_gc_resume_world();
  }
}

extern "C" {
  fn rt_gc_safepoint_slow(epoch: u64);
  #[link_name = "gc.safepoint_poll"]
  fn gc_safepoint_poll();
}

#[inline(never)]
fn enter_safepoint_slow(epoch: u64) {
  // Safety: `rt_gc_safepoint_slow` is a runtime-native entrypoint that follows the C ABI.
  unsafe {
    rt_gc_safepoint_slow(epoch);
  }
}

#[inline(never)]
fn enter_safepoint_poll() {
  // Safety: `gc.safepoint_poll` is implemented by runtime-native (arch-specific assembly). When a
  // stop-the-world epoch is requested, it captures the caller's context at the poll callsite and
  // enters the safepoint slow path.
  unsafe {
    gc_safepoint_poll();
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
  let was_registered = threading::registry::current_thread_id().is_some();
  if !was_registered {
    threading::register_current_thread(ThreadKind::Main);
  }
  struct Unregister {
    was_registered: bool,
  }
  impl Drop for Unregister {
    fn drop(&mut self) {
      if !self.was_registered {
        threading::unregister_current_thread();
      }
    }
  }
  let _unregister = Unregister { was_registered };

  let (tx_id, rx_id) = mpsc::channel();
  let (tx_epoch, rx_epoch) = mpsc::channel();

  let mut handle = JoinOnDrop::new(std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    let epoch = rx_epoch.recv().unwrap();
    enter_safepoint_slow(epoch);

    threading::unregister_current_thread();
  }));

  let worker_id = rx_id.recv().unwrap();

  let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  let _resume = ResumeWorldOnDrop;
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

  // `ctx.sp` is the *stackmap* SP (post-call). It should be at least word-aligned.
  assert_eq!(
    ctx.sp % runtime_native::arch::WORD_SIZE,
    0,
    "sp is not word-aligned"
  );
  assert_eq!(
    ctx.sp_entry % runtime_native::arch::WORD_SIZE,
    0,
    "sp_entry is not word-aligned"
  );
  assert_eq!(
    ctx.fp % runtime_native::arch::WORD_SIZE,
    0,
    "fp is not word-aligned"
  );

  // Sanity check the arch-specific SP semantics we depend on for stackmap walking.
  #[cfg(target_arch = "x86_64")]
  assert_eq!(
    ctx.sp,
    ctx.sp_entry + runtime_native::arch::WORD_SIZE,
    "x86_64 call pushes return address; expected sp=sp_entry+8"
  );
  #[cfg(target_arch = "aarch64")]
  assert_eq!(
    ctx.sp, ctx.sp_entry,
    "AArch64 bl does not push return address; expected sp=sp_entry"
  );

  let bounds = worker
    .stack_bounds()
    .expect("worker thread should have stack bounds recorded");
  assert!(
    ctx.sp >= bounds.lo && ctx.sp < bounds.hi,
    "sp {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.sp,
    bounds.lo,
    bounds.hi
  );
  assert!(
    ctx.sp_entry >= bounds.lo && ctx.sp_entry < bounds.hi,
    "sp_entry {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.sp_entry,
    bounds.lo,
    bounds.hi
  );
  assert!(
    ctx.fp >= bounds.lo && ctx.fp < bounds.hi,
    "fp {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.fp,
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

  handle.join();
}

#[test]
fn captures_mutator_ip_and_stack_pointer_via_gc_safepoint_poll() {
  let _rt = TestRuntimeGuard::new();
  let was_registered = threading::registry::current_thread_id().is_some();
  if !was_registered {
    threading::register_current_thread(ThreadKind::Main);
  }
  struct Unregister {
    was_registered: bool,
  }
  impl Drop for Unregister {
    fn drop(&mut self) {
      if !self.was_registered {
        threading::unregister_current_thread();
      }
    }
  }
  let _unregister = Unregister { was_registered };

  let (tx_id, rx_id) = mpsc::channel();
  let (tx_go, rx_go) = mpsc::channel();

  let mut handle = JoinOnDrop::new(std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    rx_go.recv().unwrap();
    enter_safepoint_poll();

    threading::unregister_current_thread();
  }));

  let worker_id = rx_id.recv().unwrap();

  // Trigger stop-the-world so the poll takes the slow path.
  let _stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  let _resume = ResumeWorldOnDrop;
  tx_go.send(()).unwrap();

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

  // `ctx.sp` is the *stackmap* SP (post-call). It should be at least word-aligned.
  assert_eq!(
    ctx.sp % runtime_native::arch::WORD_SIZE,
    0,
    "sp is not word-aligned"
  );
  assert_eq!(
    ctx.sp_entry % runtime_native::arch::WORD_SIZE,
    0,
    "sp_entry is not word-aligned"
  );
  assert_eq!(
    ctx.fp % runtime_native::arch::WORD_SIZE,
    0,
    "fp is not word-aligned"
  );

  // Sanity check the arch-specific SP semantics we depend on for stackmap walking.
  #[cfg(target_arch = "x86_64")]
  assert_eq!(
    ctx.sp,
    ctx.sp_entry + runtime_native::arch::WORD_SIZE,
    "x86_64 call pushes return address; expected sp=sp_entry+8"
  );
  #[cfg(target_arch = "aarch64")]
  assert_eq!(
    ctx.sp, ctx.sp_entry,
    "AArch64 bl does not push return address; expected sp=sp_entry"
  );

  let bounds = worker
    .stack_bounds()
    .expect("worker thread should have stack bounds recorded");
  assert!(
    ctx.sp >= bounds.lo && ctx.sp < bounds.hi,
    "sp {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.sp,
    bounds.lo,
    bounds.hi
  );
  assert!(
    ctx.sp_entry >= bounds.lo && ctx.sp_entry < bounds.hi,
    "sp_entry {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.sp_entry,
    bounds.lo,
    bounds.hi
  );
  assert!(
    ctx.fp >= bounds.lo && ctx.fp < bounds.hi,
    "fp {:#x} is outside stack bounds [{:#x}, {:#x})",
    ctx.fp,
    bounds.lo,
    bounds.hi
  );

  // The poll stub must capture the *caller* return address (into the frame that invoked
  // `gc.safepoint_poll`), not an internal runtime callsite. For this test we call it directly from
  // `enter_safepoint_poll`, so the recorded ip should resolve there.
  //
  // Use `ip - 1` to bias into the calling instruction for symbol lookup.
  let ip_for_lookup = ctx.ip.saturating_sub(1);
  if let Some(sym) = resolve_symbol_name(ip_for_lookup).filter(|s| !s.is_empty()) {
    assert!(
      sym.contains("enter_safepoint_poll"),
      "expected ip {:#x} to resolve to enter_safepoint_poll, got symbol: {sym:?}",
      ctx.ip
    );
  } else {
    // Release builds for this workspace strip symbols, so backtrace resolution can legitimately
    // fail. Fall back to a coarse range check: the return address should still fall within the
    // helper function's code range.
    let base = enter_safepoint_poll as usize;
    assert!(
      ctx.ip >= base && ctx.ip < base + 0x10_000,
      "expected ip {:#x} to fall within enter_safepoint_poll [{:#x}, {:#x})",
      ctx.ip,
      base,
      base + 0x10_000
    );
  }

  runtime_native::rt_gc_resume_world();

  handle.join();
}
