use runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG;
use runtime_native::array::RT_ARRAY_DATA_OFFSET;
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::CARD_TABLE_MIN_BYTES;
use runtime_native::test_util::TestRuntimeGuard;

fn card_table_ptr(obj: *mut u8) -> *mut std::sync::atomic::AtomicU64 {
  // SAFETY: `obj` is expected to be a valid object base pointer.
  unsafe { (&*(obj as *const ObjHeader)).card_table_ptr() }
}

#[test]
fn rt_alloc_array_installs_card_table_for_large_old_pointer_arrays() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_gc_collect();

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
fn rt_alloc_array_installs_card_table_for_promoted_old_pointer_arrays() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_gc_collect();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let ptr_elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  let len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let mut young = runtime_native::rt_alloc_array(len, ptr_elem_size);
  assert!(!young.is_null());
  assert!(
    card_table_ptr(young).is_null(),
    "young arrays must not have card tables"
  );
  let handle = runtime_native::rt_gc_register_root_slot(&mut young as *mut *mut u8);

  // A minor collection evacuates live nursery objects to old-gen, and should install card tables on
  // promoted large pointer arrays so the exported write barrier can track old→young stores.
  runtime_native::rt_gc_collect_minor();

  let old = runtime_native::rt_gc_root_get(handle);
  assert!(!old.is_null());
  assert!(
    !card_table_ptr(old).is_null(),
    "promoted old pointer arrays should receive a card table"
  );
  runtime_native::rt_gc_unregister_root_slot(handle);

  // Sanity: the tested allocation should still be Immix-eligible (i.e. not the LOS path).
  assert!(
    RT_ARRAY_DATA_OFFSET + (len * ptr_size) <= IMMIX_MAX_OBJECT_SIZE,
    "expected test array to fit in IMMIX_MAX_OBJECT_SIZE; increase filler size if this fails"
  );
}
