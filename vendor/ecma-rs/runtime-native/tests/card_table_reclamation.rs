use runtime_native::array;
use runtime_native::gc::{self, RememberedSet, RootStack, CARD_TABLE_MIN_BYTES};
use runtime_native::GcHeap;

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[cfg(any(debug_assertions, feature = "gc_debug", feature = "gc_stats"))]
#[test]
fn major_gc_reclaims_per_object_card_tables() {
  gc::reset_card_table_counters_for_tests();

  let mut heap = GcHeap::new();
  let mut roots = RootStack::new();
  let mut remembered = NullRememberedSet::default();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let elem_size = array::RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  // Array that is large enough to install a card table but still allocated in
  // Immix (not LOS) under the default heap config.
  let len_immix = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);

  // Array large enough to force LOS allocation (default los_threshold_bytes is
  // 8 KiB).
  let len_los = (8 * 1024usize).div_ceil(ptr_size);

  const CYCLES: usize = 3;
  const IMMIX_ARRAYS_PER_CYCLE: usize = 512;
  const LOS_ARRAYS_PER_CYCLE: usize = 64;

  for _ in 0..CYCLES {
    let alloc_before = gc::card_table_bytes_allocated_for_tests();
    let free_before = gc::card_table_bytes_freed_for_tests();

    for _ in 0..IMMIX_ARRAYS_PER_CYCLE {
      heap.alloc_array_old(len_immix, elem_size);
    }
    for _ in 0..LOS_ARRAYS_PER_CYCLE {
      heap.alloc_array_old(len_los, elem_size);
    }

    let alloc_after = gc::card_table_bytes_allocated_for_tests();
    assert!(
      alloc_after > alloc_before,
      "expected card table allocations to increase while allocating arrays"
    );

    heap.collect_major(&mut roots, &mut remembered).unwrap();

    let free_after = gc::card_table_bytes_freed_for_tests();
    assert_eq!(
      free_after - free_before,
      alloc_after - alloc_before,
      "expected major GC to reclaim all card tables for unreachable arrays"
    );
  }

  assert!(
    gc::card_table_bytes_allocated_for_tests() > 0,
    "test should allocate at least one card table"
  );
  assert_eq!(
    gc::card_table_bytes_allocated_for_tests(),
    gc::card_table_bytes_freed_for_tests(),
    "card table bytes should not leak across major GC cycles"
  );
}

