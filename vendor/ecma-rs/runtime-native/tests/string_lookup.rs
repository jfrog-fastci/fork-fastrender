use runtime_native::abi::InternedId;
use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn string_lookup_pinned_roundtrips_for_pinned_ids() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = runtime_native::rt_string_intern(b"perm".as_ptr(), b"perm".len());
    runtime_native::rt_string_pin_interned(id);

    let mut out = runtime_native::StringRef::empty();
    let ok = unsafe { runtime_native::rt_string_lookup_pinned(id, &mut out) };
    assert!(ok);
    // Safety: `rt_string_lookup_pinned` returned a valid byte slice for the returned length.
    let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
    assert_eq!(bytes, b"perm");

    // Pinned strings should remain lookuppable even after a GC/prune cycle.
    runtime_native::test_util::interner_collect_garbage_for_tests();
    let mut out2 = runtime_native::StringRef::empty();
    let ok2 = unsafe { runtime_native::rt_string_lookup_pinned(id, &mut out2) };
    assert!(ok2);
    // Safety: `rt_string_lookup_pinned` returned a valid byte slice for the returned length.
    let bytes2 = unsafe { std::slice::from_raw_parts(out2.ptr, out2.len) };
    assert_eq!(bytes2, b"perm");
  });
}

#[test]
fn string_lookup_pinned_fails_for_unpinned_or_pruned_ids() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = runtime_native::rt_string_intern(b"temp".as_ptr(), b"temp".len());

    let mut out = runtime_native::StringRef::empty();
    let ok = unsafe { runtime_native::rt_string_lookup_pinned(id, &mut out) };
    assert!(
      !ok,
      "unpinned entries must not expose GC-backed pointers via rt_string_lookup_pinned"
    );
    assert_eq!(out.len, 0);

    // Force interner GC/prune: since the entry is not pinned, it can be reclaimed and the ID becomes
    // invalid.
    runtime_native::test_util::interner_collect_garbage_for_tests();

    let mut out2 = runtime_native::StringRef::empty();
    let ok2 = unsafe { runtime_native::rt_string_lookup_pinned(id, &mut out2) };
    assert!(!ok2);
    assert_eq!(out2.len, 0);
  });
}

#[test]
fn string_lookup_roundtrips_non_empty_string() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let expected = b"hello";
    let id = runtime_native::rt_string_intern(expected.as_ptr(), expected.len());
    let got = runtime_native::rt_string_lookup(id);

    assert!(!got.ptr.is_null());
    assert_eq!(got.len, expected.len());

    // Safety: `rt_string_lookup` returned a non-null byte pointer for `got.len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(got.ptr, got.len) };
    assert_eq!(bytes, expected);
  });
}

#[test]
fn string_lookup_invalid_id_returns_distinguishable_null_stringref() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let got = runtime_native::rt_string_lookup(InternedId::INVALID);
    assert!(got.ptr.is_null());
    assert_eq!(got.len, 0);
  });
}

#[test]
fn string_lookup_empty_string_is_distinct_from_invalid() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = runtime_native::rt_string_intern(std::ptr::null(), 0);
    let got = runtime_native::rt_string_lookup(id);

    assert_eq!(got.len, 0);
    assert!(
      !got.ptr.is_null(),
      "valid empty string must return a non-null pointer (distinct from invalid ID sentinel)"
    );

    // Safety: `got` is a valid empty slice reference (ptr is non-null and `u8` has alignment 1).
    let bytes = unsafe { std::slice::from_raw_parts(got.ptr, got.len) };
    assert!(bytes.is_empty());
  });
}

