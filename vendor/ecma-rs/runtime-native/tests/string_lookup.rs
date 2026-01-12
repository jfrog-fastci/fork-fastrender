use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn string_lookup_roundtrips_for_pinned_ids() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = runtime_native::rt_string_intern(b"perm".as_ptr(), b"perm".len());
    runtime_native::rt_string_pin_interned(id);

    let mut out = runtime_native::StringRef::empty();
    let ok = unsafe { runtime_native::rt_string_lookup(id, &mut out) };
    assert!(ok);
    // Safety: `rt_string_lookup` returns a valid byte slice for the returned length.
    let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
    assert_eq!(bytes, b"perm");

    // Pinned strings should remain lookuppable even after the interner's internal heap GC/prune.
    runtime_native::test_util::interner_collect_garbage_for_tests();
    let mut out2 = runtime_native::StringRef::empty();
    let ok2 = unsafe { runtime_native::rt_string_lookup(id, &mut out2) };
    assert!(ok2);
    // Safety: `rt_string_lookup` returns a valid byte slice for the returned length.
    let bytes2 = unsafe { std::slice::from_raw_parts(out2.ptr, out2.len) };
    assert_eq!(bytes2, b"perm");
  });
}

#[test]
fn string_lookup_fails_for_unpinned_or_pruned_ids() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = runtime_native::rt_string_intern(b"temp".as_ptr(), b"temp".len());

    let mut out = runtime_native::StringRef::empty();
    let ok = unsafe { runtime_native::rt_string_lookup(id, &mut out) };
    assert!(!ok, "unpinned entries must not expose GC-backed pointers via rt_string_lookup");
    assert_eq!(out.len, 0);

    // Force interner GC/prune: since the entry is not pinned, it can be reclaimed and the ID becomes
    // invalid.
    runtime_native::test_util::interner_collect_garbage_for_tests();

    let mut out2 = runtime_native::StringRef::empty();
    let ok2 = unsafe { runtime_native::rt_string_lookup(id, &mut out2) };
    assert!(!ok2);
    assert_eq!(out2.len, 0);
  });
}

