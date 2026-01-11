use runtime_native::roots::RootScope;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

#[test]
#[cfg(debug_assertions)]
fn gc_safe_requires_empty_handle_stack() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let mut scope = RootScope::new();
    let mut slot = std::ptr::null_mut::<u8>();
    scope.push(&mut slot as *mut *mut u8);
    let _guard = threading::enter_gc_safe_region();
  }));

  threading::unregister_current_thread();
  assert!(
    result.is_err(),
    "expected debug assertion when entering a GC-safe region with handle-stack roots"
  );
}

