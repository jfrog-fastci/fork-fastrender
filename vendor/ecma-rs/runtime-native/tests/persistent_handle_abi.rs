use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn persistent_handle_abi_roundtrip() {
  let _rt = TestRuntimeGuard::new();

  let a = Box::into_raw(Box::new(1u8)) as *mut u8;
  let b = Box::into_raw(Box::new(2u8)) as *mut u8;

  let h = runtime_native::rt_handle_alloc(a);
  assert_eq!(runtime_native::rt_handle_load(h), a);

  runtime_native::rt_handle_store(h, b);
  assert_eq!(runtime_native::rt_handle_load(h), b);

  runtime_native::rt_handle_free(h);
  assert_eq!(runtime_native::rt_handle_load(h), std::ptr::null_mut());
  // Double-free should be a no-op.
  runtime_native::rt_handle_free(h);

  unsafe {
    drop(Box::from_raw(a));
    drop(Box::from_raw(b));
  }
}
