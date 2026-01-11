use runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG;
use runtime_native::array::RT_ARRAY_DATA_OFFSET;
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::CARD_TABLE_MIN_BYTES;

fn card_table_ptr(obj: *mut u8) -> *mut std::sync::atomic::AtomicU64 {
  // SAFETY: `obj` is expected to be a valid object base pointer.
  unsafe { (&*(obj as *const ObjHeader)).card_table_ptr() }
}

#[test]
fn rt_alloc_array_installs_card_table_for_large_old_pointer_arrays() {
  let ptr_size = core::mem::size_of::<*mut u8>();
  let ptr_elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  // Large pointer array that still fits in Immix and should allocate in the nursery.
  let young_len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let young = runtime_native::rt_alloc_array(young_len, ptr_elem_size);
  assert!(!young.is_null());
  assert!(card_table_ptr(young).is_null(), "young arrays must not have card tables");

  // Force old-generation allocation via LOS by exceeding the maximum Immix size.
  let old_len = (IMMIX_MAX_OBJECT_SIZE / ptr_size) + 1;
  let old = runtime_native::rt_alloc_array(old_len, ptr_elem_size);
  assert!(!old.is_null());
  assert!(
    !card_table_ptr(old).is_null(),
    "old large pointer arrays should receive a card table"
  );

  // Non-pointer arrays must not get a card table even when allocated in old-gen.
  let old_non_ptr = runtime_native::rt_alloc_array(old_len, ptr_size);
  assert!(!old_non_ptr.is_null());
  assert!(
    card_table_ptr(old_non_ptr).is_null(),
    "non-pointer arrays should not receive card tables"
  );
}

#[test]
fn rt_alloc_array_old_fallback_installs_card_table_once_nursery_is_exhausted() {
  let ptr_size = core::mem::size_of::<*mut u8>();
  let ptr_elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  // Ensure the global heap is initialized so `rt_gc_get_young_range` returns a meaningful range.
  let init = runtime_native::rt_alloc_array(1, 1);
  assert!(!init.is_null());

  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut start, &mut end);
  }
  assert!(!start.is_null(), "expected nursery range start to be initialized");
  assert!(!end.is_null(), "expected nursery range end to be initialized");
  assert!(start < end, "invalid nursery range");

  // Exhaust the nursery with non-pointer arrays that are the *same total size* as the pointer array
  // we want to test below:
  // - payload bytes: CARD_TABLE_MIN_BYTES
  // - total bytes: RT_ARRAY_DATA_OFFSET + CARD_TABLE_MIN_BYTES
  //
  // Once such an allocation falls back to old-gen, we know the nursery has < that many bytes left,
  // so the next allocation of the (same-size) pointer array must also fall back.
  let filler_len = CARD_TABLE_MIN_BYTES;
  let filler_total = RT_ARRAY_DATA_OFFSET + filler_len;
  let nursery_bytes = (end as usize).saturating_sub(start as usize);
  let max_iters = nursery_bytes / filler_total + 1024;
  let mut saw_old_alloc = false;
  for _ in 0..max_iters {
    let obj = runtime_native::rt_alloc_array(filler_len, 1);
    let addr = obj as usize;
    if addr < start as usize || addr >= end as usize {
      saw_old_alloc = true;
      break;
    }
  }
  assert!(
    saw_old_alloc,
    "expected to exhaust nursery after <= {max_iters} allocations of {filler_total} bytes"
  );

  // Allocate a large pointer array that still fits in Immix. Since the nursery is exhausted, this
  // must take the old-gen fallback path and should receive a per-object card table.
  let len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let old = runtime_native::rt_alloc_array(len, ptr_elem_size);
  assert!(!old.is_null());
  let addr = old as usize;
  assert!(
    addr < start as usize || addr >= end as usize,
    "expected allocation to fall back to old-gen after nursery exhaustion"
  );
  assert!(
    !card_table_ptr(old).is_null(),
    "old-gen fallback pointer arrays should receive a card table"
  );

  // Sanity: the tested allocation should still be Immix-eligible (i.e. not the LOS path).
  assert!(
    RT_ARRAY_DATA_OFFSET + (len * ptr_size) <= IMMIX_MAX_OBJECT_SIZE,
    "expected test array to fit in IMMIX_MAX_OBJECT_SIZE; increase filler size if this fails"
  );
}
