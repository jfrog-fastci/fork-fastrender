use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_gc_collect, rt_string_intern, rt_string_pin_interned, rt_string_lookup, StringRef};
use std::ptr;
 
#[test]
fn unpinned_interned_string_can_be_reclaimed() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::test_util::with_interner_test_lock(|| {
    let id = rt_string_intern(b"temp".as_ptr(), b"temp".len());
    assert!(
      runtime_native::test_util::interner_lookup_exists(id),
      "expected interned id {id:?} to be live before GC"
    );
 
    rt_gc_collect();
 
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
    let ok_before = unsafe { rt_string_lookup(id, &mut before) };
    assert!(ok_before);
    // Safety: `rt_string_lookup` returns a valid byte slice for the returned length.
    let bytes_before = unsafe { std::slice::from_raw_parts(before.ptr, before.len) };
    assert_eq!(bytes_before, b"pinned");
 
    rt_gc_collect();
 
    let mut after = StringRef::empty();
    let ok_after = unsafe { rt_string_lookup(id, &mut after) };
    assert!(ok_after);
    // Safety: `rt_string_lookup` returns a valid byte slice for the returned length.
    let bytes_after = unsafe { std::slice::from_raw_parts(after.ptr, after.len) };
    assert_eq!(bytes_after, b"pinned");
 
    // Pinned lookups must return stable non-GC pointers.
    assert_ne!(after.ptr, ptr::null());
    assert_eq!(after.ptr, before.ptr);
    assert_eq!(after.len, before.len);
  });
}
