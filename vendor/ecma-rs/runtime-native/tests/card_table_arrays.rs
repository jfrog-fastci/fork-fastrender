use std::sync::atomic::Ordering;

use runtime_native::array;
use runtime_native::gc::{ObjHeader, TypeDescriptor, CARD_SIZE};
use runtime_native::test_util::TestGcGuard;
use runtime_native::{rt_gc_set_young_range, rt_write_barrier, rt_write_barrier_range, GcHeap, RememberedSet, RootStack};

static LEAF_DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<ObjHeader>(), &[]);

struct SingleRemembered {
  obj: *mut u8,
}

impl RememberedSet for SingleRemembered {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    if !self.obj.is_null() {
      f(self.obj);
    }
  }

  fn clear(&mut self) {
    // `GcHeap::collect_minor` calls `RememberedSet::clear` after evacuating the entire nursery.
    // Tests that provide synthetic remembered sets don't need to model remembered-bit cleanup.
  }

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn pointer_array_card_table_marks_and_scans_only_dirty_cards() {
  // Serialize with other tests that mutate the exported write barrier's global state.
  let _guard = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut heap = GcHeap::new();

  // Allocate a pointer array large enough to justify installing a card table.
  //
  // Policy: >= 8 cards worth of pointers (>= 4 KiB with 512 B cards).
  let ptr_size = core::mem::size_of::<*mut u8>();
  let len = (8 * CARD_SIZE) / ptr_size * 2;
  let array = heap.alloc_array_young(len, ptr_size | array::RT_ARRAY_ELEM_PTR_FLAG);

  // Promote the array into old-gen so the exported write barrier treats it as an "old" object.
  let mut root_array = array;
  let mut roots = RootStack::new();
  roots.push(&mut root_array as *mut *mut u8);
  let mut remembered = SingleRemembered { obj: core::ptr::null_mut() };
  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  let array_old = root_array;
  assert!(!heap.is_in_nursery(array_old));

  // Card table should be installed for this array.
  let header = unsafe { &*(array_old as *const ObjHeader) };
  let card_table = header.card_table_ptr();
  assert!(!card_table.is_null(), "expected card table to be installed for large pointer array");

  // Allocate two young objects.
  let y0 = heap.alloc_young(&LEAF_DESC);
  let y1 = heap.alloc_young(&LEAF_DESC);

  // Configure the exported write barrier's young-range to include just these test objects.
  let start = (y0 as usize).min(y1 as usize) as *mut u8;
  let end = ((y0 as usize).max(y1 as usize) + 1) as *mut u8;
  rt_gc_set_young_range(start, end);

  // Pick two element indices in different cards (card 1 and card 2).
  let first_card1_idx = (CARD_SIZE - array::RT_ARRAY_DATA_OFFSET).div_ceil(ptr_size);
  let idx_marked = first_card1_idx;
  let idx_unmarked = idx_marked + (CARD_SIZE / ptr_size);
  assert!(idx_unmarked < len, "test array must span at least two payload cards");

  // Store `y0` into card 1 and run the per-slot write barrier (marks the card).
  let slots = unsafe { array::array_data_ptr(array_old).cast::<*mut u8>() };
  let slot0 = unsafe { slots.add(idx_marked) };
  unsafe {
    slot0.write(y0);
    rt_write_barrier(array_old, slot0 as *mut u8);
  }

  // Verify the correct card bit was marked.
  let slot0_offset = (slot0 as usize) - (array_old as usize);
  let card0 = slot0_offset / CARD_SIZE;
  let word_idx = card0 / 64;
  let bit = card0 % 64;
  let word = unsafe { (*card_table.add(word_idx)).load(Ordering::Acquire) };
  assert_ne!(word & (1u64 << bit), 0, "expected card {card0} to be marked");

  // Store `y1` into a different card but *do not* mark it.
  let slot1 = unsafe { slots.add(idx_unmarked) };
  unsafe {
    slot1.write(y1);
  }

  // Run a minor GC that scans the old array via its dirty cards.
  //
  // Root `y1` independently so it is evacuated even if the array's unmarked card is not scanned.
  let mut root_y1 = y1;
  let mut roots = RootStack::new();
  roots.push(&mut root_array as *mut *mut u8);
  roots.push(&mut root_y1 as *mut *mut u8);

  let mut remembered = SingleRemembered { obj: array_old };
  heap.collect_minor(&mut roots, &mut remembered).unwrap();

  // The slot in the marked card must have been scanned/updated to point at the evacuated object.
  let slot0_after = unsafe { slot0.read() };
  assert!(
    !heap.is_in_nursery(slot0_after),
    "expected slot in marked card to be updated to a promoted pointer"
  );

  // The slot in the unmarked card must not have been visited/updated.
  let slot1_after = unsafe { slot1.read() };
  assert!(
    heap.is_in_nursery(slot1_after),
    "expected slot in unmarked card to remain a nursery pointer (card was not scanned)"
  );

  // Scanning clears dirty card bits.
  let word_after = unsafe { (*card_table.add(word_idx)).load(Ordering::Acquire) };
  assert_eq!(word_after, 0, "expected dirty card bits to be cleared after scan");
}

#[test]
fn write_barrier_range_marks_all_cards_spanning_the_range() {
  let _guard = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut heap = GcHeap::new();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let len = (8 * CARD_SIZE) / ptr_size * 2;
  let array = heap.alloc_array_young(len, ptr_size | array::RT_ARRAY_ELEM_PTR_FLAG);

  // Promote to old-gen so the range barrier takes the old-object path.
  let mut root_array = array;
  let mut roots = RootStack::new();
  roots.push(&mut root_array as *mut *mut u8);
  let mut remembered = SingleRemembered { obj: core::ptr::null_mut() };
  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  let array_old = root_array;

  let header = unsafe { &*(array_old as *const ObjHeader) };
  let card_table = header.card_table_ptr();
  assert!(!card_table.is_null());

  // Mark a range that spans cards 1 and 2.
  let start = unsafe { (array_old as *mut u8).add(CARD_SIZE) };
  let len_bytes = 2 * CARD_SIZE;
  unsafe {
    rt_write_barrier_range(array_old, start, len_bytes);
  }

  let word = unsafe { (*card_table).load(Ordering::Acquire) };
  assert_ne!(word & (1u64 << 1), 0, "expected card 1 to be marked");
  assert_ne!(word & (1u64 << 2), 0, "expected card 2 to be marked");
}
