use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::StringRef;

#[test]
fn string_concat_roundtrip_and_free() {
  let _rt = TestRuntimeGuard::new();

  let a = b"hello";
  let b = b" world";

  let s = runtime_native::rt_string_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len());
  assert_eq!(s.len, a.len() + b.len());

  unsafe {
    let bytes = std::slice::from_raw_parts(s.ptr, s.len);
    assert_eq!(bytes, b"hello world");
  }

  runtime_native::rt_string_free(s);

  // `rt_string_free` must be idempotent-safe for `{NULL, 0}` and generally safe for empty strings.
  runtime_native::rt_string_free(StringRef {
    ptr: std::ptr::null(),
    len: 0,
  });
  runtime_native::rt_string_free(StringRef::empty());

  // Allocate+free repeatedly so Miri/ASAN/LSAN runs have a chance to catch leaks or allocator misuse.
  for _ in 0..10_000 {
    let s = runtime_native::rt_string_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len());
    runtime_native::rt_string_free(s);
  }

  // Ensure `rt_string_concat` still allows null pointers when lengths are zero.
  let empty = runtime_native::rt_string_concat(std::ptr::null(), 0, std::ptr::null(), 0);
  assert_eq!(empty.len, 0);
  runtime_native::rt_string_free(empty);
}

