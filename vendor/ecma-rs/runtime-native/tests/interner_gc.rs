use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_gc_collect, rt_string_intern, rt_string_lookup, rt_string_lookup_pinned, rt_string_pin_interned,
  StringRef,
};
use std::ptr;
  
#[test]
fn unpinned_interned_string_can_be_reclaimed() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = rt_string_intern(b"temp".as_ptr(), b"temp".len());

    let before = rt_string_lookup(id);
    assert!(
      !before.ptr.is_null(),
      "expected rt_string_lookup({id:?}) to succeed before GC"
    );
    // Safety: `rt_string_lookup` returned a non-null byte pointer for `before.len` bytes.
    let bytes_before = unsafe { std::slice::from_raw_parts(before.ptr, before.len) };
    assert_eq!(bytes_before, b"temp");
    assert!(
      runtime_native::test_util::interner_lookup_exists(id),
      "expected interned id {id:?} to be live before GC"
    );
  
    rt_gc_collect();
  
    let after = rt_string_lookup(id);
    assert!(
      after.ptr.is_null(),
      "expected rt_string_lookup({id:?}) to return NULL after GC reclamation"
    );
    assert_eq!(after.len, 0);
    assert!(
      !runtime_native::test_util::interner_lookup_exists(id),
      "expected interned id {id:?} to be reclaimed after GC"
    );
  });
}
  
#[test]
fn pinned_interned_string_survives_gc() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = rt_string_intern(b"pinned".as_ptr(), b"pinned".len());
    rt_string_pin_interned(id);
  
    let mut before = StringRef::empty();
    let ok_before = unsafe { rt_string_lookup_pinned(id, &mut before) };
    assert!(ok_before);
    // Safety: `rt_string_lookup_pinned` returned a valid byte slice for the returned length.
    let bytes_before = unsafe { std::slice::from_raw_parts(before.ptr, before.len) };
    assert_eq!(bytes_before, b"pinned");
  
    rt_gc_collect();
  
    let mut after = StringRef::empty();
    let ok_after = unsafe { rt_string_lookup_pinned(id, &mut after) };
    assert!(ok_after);
    // Safety: `rt_string_lookup_pinned` returned a valid byte slice for the returned length.
    let bytes_after = unsafe { std::slice::from_raw_parts(after.ptr, after.len) };
    assert_eq!(bytes_after, b"pinned");
  
    // Pinned lookups must return stable non-GC pointers.
    assert_ne!(after.ptr, ptr::null());
    assert_eq!(after.ptr, before.ptr);
    assert_eq!(after.len, before.len);
  });
}

#[test]
fn reinterning_after_reclamation_returns_new_id() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id1 = rt_string_intern(b"temp".as_ptr(), b"temp".len());
    let before = rt_string_lookup(id1);
    assert!(!before.ptr.is_null());
    // Safety: `rt_string_lookup` returned a non-null byte pointer for `before.len` bytes.
    let bytes_before = unsafe { std::slice::from_raw_parts(before.ptr, before.len) };
    assert_eq!(bytes_before, b"temp");

    // After GC, the unpinned interned entry may be reclaimed and the ID becomes permanently invalid.
    rt_gc_collect();
    let after = rt_string_lookup(id1);
    assert!(after.ptr.is_null());
    assert_eq!(after.len, 0);

    // Re-interning the same bytes yields a new monotonic ID; IDs are never reused.
    let id2 = rt_string_intern(b"temp".as_ptr(), b"temp".len());
    assert_ne!(id2, id1);
    let got2 = rt_string_lookup(id2);
    assert!(!got2.ptr.is_null());
    // Safety: `rt_string_lookup` returned a non-null byte pointer for `got2.len` bytes.
    let bytes2 = unsafe { std::slice::from_raw_parts(got2.ptr, got2.len) };
    assert_eq!(bytes2, b"temp");
  });
}
