use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::threading::safepoint::RT_GC_EPOCH as RT_GC_EPOCH_RUST;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

extern "C" {
  static RT_GC_EPOCH: AtomicU64;
}

#[test]
fn gc_epoch_symbol_is_link_visible_and_controls_stw() {
  let _rt = TestRuntimeGuard::new();
  // The exported symbol must be stable and link-visible (no Rust name mangling).
  let rust_addr = (&RT_GC_EPOCH_RUST as *const AtomicU64) as usize;
  let ffi_addr = unsafe { (&RT_GC_EPOCH as *const AtomicU64) as usize };
  assert_eq!(rust_addr, ffi_addr, "RT_GC_EPOCH should resolve via `extern`");

  // The epoch must be mutable from Rust.
  RT_GC_EPOCH_RUST.store(0, Ordering::SeqCst);
  assert_eq!(unsafe { RT_GC_EPOCH.load(Ordering::SeqCst) }, 0);
  unsafe {
    RT_GC_EPOCH.store(6, Ordering::SeqCst);
  }
  assert_eq!(RT_GC_EPOCH_RUST.load(Ordering::SeqCst), 6);

  // Stop-the-world helpers still behave correctly (odd = stop, even = resume).
  threading::register_current_thread(ThreadKind::Main);

  let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch should be odd");
  assert_eq!(RT_GC_EPOCH_RUST.load(Ordering::SeqCst), stop_epoch);
  assert_eq!(unsafe { RT_GC_EPOCH.load(Ordering::SeqCst) }, stop_epoch);

  // With no other registered threads, the world is trivially stopped.
  runtime_native::rt_gc_wait_for_world_stopped();

  let resume_epoch = runtime_native::rt_gc_resume_world();
  assert_eq!(resume_epoch & 1, 0, "resumed epoch should be even");
  assert_eq!(resume_epoch, stop_epoch + 1);
  assert_eq!(RT_GC_EPOCH_RUST.load(Ordering::SeqCst), resume_epoch);

  threading::unregister_current_thread();
}
