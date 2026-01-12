use runtime_native::array::RT_ARRAY_DATA_OFFSET;
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc_array, rt_gc_get_young_range, rt_gc_register_root_slot, rt_gc_unregister_root_slot, rt_thread_deinit,
  rt_thread_init, rt_gc_root_get,
};

#[test]
fn rt_alloc_triggers_minor_gc_without_explicit_collect() {
  let _rt = TestRuntimeGuard::new();
  // Register this thread so stop-the-world coordination considers it a mutator.
  rt_thread_init(3);

  // Construct an array allocation size right at the nursery/Immix max so the nursery fills quickly.
  // Use a non-pointer payload to avoid card-table paths and keep the test focused on nursery GC.
  let elem_size = 1usize;
  let len = IMMIX_MAX_OBJECT_SIZE
    .checked_sub(RT_ARRAY_DATA_OFFSET)
    .expect("array header larger than IMMIX_MAX_OBJECT_SIZE");

  // Capture the current nursery range.
  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null(), "expected young range to be initialized");
  assert!(!young_end.is_null(), "expected young range to be initialized");
  assert!(young_start < young_end, "invalid young range");

  // Allocate a rooted nursery object.
  let mut rooted = rt_alloc_array(len, elem_size);
  assert!(!rooted.is_null());
  // The runtime performs conservative stack scanning fallback in debug builds when stackmaps are
  // unavailable for Rust frames. That can treat any stack word that *looks* like a young pointer as
  // a root slot and update it during evacuation, including locals we use for test bookkeeping.
  //
  // Tag the original pointer value so it is no longer aligned and won't be mistaken for an object
  // base pointer during conservative scanning.
  let rooted_before_tagged = (rooted as usize) | 1;
  assert!(
    (young_start as usize..young_end as usize).contains(&(rooted as usize)),
    "expected rooted object to be allocated in the nursery"
  );
  let handle = rt_gc_register_root_slot(&mut rooted as *mut *mut u8);

  // Allocate enough additional nursery objects to exceed the nursery usage threshold and force a
  // stop-the-world minor collection initiated implicitly by `rt_alloc_array`.
  //
  // We intentionally allocate more than a full nursery's worth of bytes so the test remains correct
  // whether the allocator triggers minor GC at a configured threshold (e.g. 80%) or only on nursery
  // exhaustion.
  let nursery_bytes = (young_end as usize).saturating_sub(young_start as usize);
  let allocs = nursery_bytes / IMMIX_MAX_OBJECT_SIZE + 512;
  for _ in 0..allocs {
    let _ = rt_alloc_array(len, elem_size);
  }

  // Minor GC may conservatively scan and mutate stack slots that *look* like GC pointers (expected
  // in debug builds when stackmaps are unavailable for Rust frames). Re-read the young range after
  // the allocation storm so the assertion isn't comparing against stack-scribbled locals.
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }

  let rooted_after = rt_gc_root_get(handle) as usize;
  assert_ne!(
    rooted_after,
    rooted_before_tagged & !1,
    "expected rooted pointer to be relocated by allocator-triggered minor GC"
  );
  assert!(
    !(young_start as usize..young_end as usize).contains(&rooted_after),
    "expected rooted object to be evacuated to old-gen by allocator-triggered minor GC"
  );

  rt_gc_unregister_root_slot(handle);
  rt_thread_deinit();
}
