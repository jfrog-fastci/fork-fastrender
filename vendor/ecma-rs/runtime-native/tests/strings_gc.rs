use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_gc_collect, rt_gc_get_young_range, rt_root_pop, rt_root_push, rt_string_as_utf8, rt_string_concat,
  rt_string_concat_gc, rt_string_len, rt_string_new_utf8, rt_string_to_owned_utf8, rt_stringref_free,
  rt_thread_deinit, rt_thread_init, rt_weak_add, rt_weak_get, rt_weak_remove,
};

fn nursery_contains(ptr: *mut u8) -> bool {
  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  // SAFETY: out pointers are valid.
  unsafe { rt_gc_get_young_range(&mut start, &mut end) };
  let addr = ptr as usize;
  addr >= start as usize && addr < end as usize
}

#[test]
fn rt_string_concat_returns_owned_bytes_and_is_freeable() {
  let _rt = TestRuntimeGuard::new();

  let out = rt_string_concat(b"foo".as_ptr(), 3, b"bar".as_ptr(), 3);
  assert_eq!(out.len, 6);
  // SAFETY: `rt_string_concat` returns a valid byte slice for the returned length.
  let bytes = unsafe { core::slice::from_raw_parts(out.ptr, out.len) };
  assert_eq!(bytes, b"foobar");
  rt_stringref_free(out);
}

#[test]
fn gc_strings_concat_and_decode() {
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(0);

  let a = rt_string_new_utf8(b"foo".as_ptr(), 3);
  let b = rt_string_new_utf8(b"bar".as_ptr(), 3);
  let ab = rt_string_concat_gc(a, b);
  assert_eq!(rt_string_len(ab), 6);

  let view = rt_string_as_utf8(ab);
  // SAFETY: `rt_string_as_utf8` returns a valid byte range until the next GC.
  let bytes = unsafe { core::slice::from_raw_parts(view.ptr, view.len) };
  assert_eq!(bytes, b"foobar");

  // Owned copy must be explicitly freed.
  let owned = rt_string_to_owned_utf8(ab);
  let owned_bytes = unsafe { core::slice::from_raw_parts(owned.ptr, owned.len) };
  assert_eq!(owned_bytes, b"foobar");
  rt_stringref_free(owned);

  rt_thread_deinit();
}

#[test]
fn gc_string_survives_collection_and_relocates() {
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(0);

  let s = rt_string_new_utf8(b"hello".as_ptr(), 5);
  assert!(nursery_contains(s), "expected new strings to be allocated into the nursery");

  let mut root = s;
  // SAFETY: `root` is a valid `GcPtr` slot and is popped in LIFO order.
  unsafe { rt_root_push(&mut root as *mut *mut u8) };

  // Conservative scanning fallback (when enabled) can treat stack words as candidate pointers.
  // Tag the old value so it is not a plausible aligned pointer.
  let before_tagged = (root as usize) | 1;
  rt_gc_collect();

  let after = root;
  assert!(
    !nursery_contains(after),
    "rooted string should be evacuated out of the nursery during collection"
  );

  let before = (before_tagged & !1) as *mut u8;
  assert_ne!(after, before, "expected a nursery allocation to relocate during GC");

  let view = rt_string_as_utf8(after);
  // SAFETY: `rt_string_as_utf8` returns a valid byte range until the next GC.
  let bytes = unsafe { core::slice::from_raw_parts(view.ptr, view.len) };
  assert_eq!(bytes, b"hello");

  // SAFETY: the pushed slot pointer is still valid and is the most recent root.
  unsafe { rt_root_pop(&mut root as *mut *mut u8) };

  rt_thread_deinit();
}

#[test]
fn gc_string_is_collectible_via_weak_handle() {
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(0);

  let mut s = rt_string_new_utf8(b"dead".as_ptr(), 4);
  let weak = rt_weak_add(s);

  // Ensure the aligned object pointer bits are not present as a plausible conservative root.
  let tagged = (s as usize) | 1;
  s = core::ptr::null_mut();
  core::hint::black_box(s);
  core::hint::black_box(tagged);

  rt_gc_collect();
  assert!(rt_weak_get(weak).is_null(), "unrooted string should be collected");
  rt_weak_remove(weak);

  rt_thread_deinit();
}
