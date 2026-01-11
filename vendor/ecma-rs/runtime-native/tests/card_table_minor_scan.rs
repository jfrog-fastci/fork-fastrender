use core::alloc::Layout;
use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use runtime_native::array;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::gc::CARD_SIZE;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

struct CardTableAlloc {
  ptr: *mut AtomicU64,
  layout: Layout,
  words: usize,
}

impl CardTableAlloc {
  fn new(words: usize) -> Self {
    assert!(words > 0);
    let layout =
      Layout::from_size_align(words * mem::size_of::<AtomicU64>(), 16).expect("invalid layout");
    // SAFETY: layout has non-zero size.
    let raw = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
    if raw.is_null() {
      std::alloc::handle_alloc_error(layout);
    }
    Self {
      ptr: raw,
      layout,
      words,
    }
  }

  fn as_ptr(&self) -> *mut AtomicU64 {
    self.ptr
  }

  fn word(&self, idx: usize) -> &AtomicU64 {
    assert!(idx < self.words);
    // SAFETY: allocation contains `words` `AtomicU64`s.
    unsafe { &*self.ptr.add(idx) }
  }
}

impl Drop for CardTableAlloc {
  fn drop(&mut self) {
    // SAFETY: `ptr` was allocated with `layout`.
    unsafe {
      std::alloc::dealloc(self.ptr as *mut u8, self.layout);
    }
  }
}

fn card_for_array_elem_index(index: usize) -> usize {
  let slot_offset = array::RT_ARRAY_DATA_OFFSET + index * mem::size_of::<*mut u8>();
  slot_offset / CARD_SIZE
}

#[test]
fn minor_gc_scans_only_dirty_cards_for_pointer_arrays_and_clears_bits() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  // Ensure the array spans multiple cards. For pointer arrays, card 0 contains
  // fewer slots due to the array header, so pick a length comfortably > 64.
  const LEN: usize = 128;

  let encoded_ptr_elem_size = array::RT_ARRAY_ELEM_PTR_FLAG | mem::size_of::<*mut u8>();
  let arr_young = heap.alloc_array_young(LEN, encoded_ptr_elem_size);

  // Evacuate the array into old-gen.
  let mut root_arr = arr_young;
  let mut roots = RootStack::new();
  roots.push(&mut root_arr as *mut *mut u8);
  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .expect("minor GC");

  let arr_old = root_arr;
  assert!(!heap.is_in_nursery(arr_old));

  // Install a per-object card table on the old array.
  let obj_size = array::RT_ARRAY_DATA_OFFSET + LEN * mem::size_of::<*mut u8>();
  let card_count = obj_size.div_ceil(CARD_SIZE);
  let word_count = card_count.div_ceil(64);
  let card_table = CardTableAlloc::new(word_count);
  unsafe {
    let header = &mut *(arr_old as *mut ObjHeader);
    header.set_card_table_ptr(card_table.as_ptr());
  }

  // Create three nursery objects and write them into three distinct cards.
  let young_marked_0 = heap.alloc_young(&NODE_DESC);
  let young_unmarked = heap.alloc_young(&NODE_DESC);
  let young_marked_2 = heap.alloc_young(&NODE_DESC);

  unsafe {
    let data = arr_old.add(array::RT_ARRAY_DATA_OFFSET) as *mut *mut u8;
    *data.add(10) = young_marked_0;
    *data.add(70) = young_unmarked;
    *data.add(126) = young_marked_2;
  }

  // Mark only the cards for indices 10 and 126. Index 70 is deliberately left
  // unmarked to prove scanning is card-driven (simulates a missing barrier).
  let card_0 = card_for_array_elem_index(10);
  let card_1 = card_for_array_elem_index(70);
  let card_2 = card_for_array_elem_index(126);
  assert_ne!(card_0, card_1);
  assert_ne!(card_1, card_2);

  card_table
    .word(0)
    .fetch_or((1u64 << card_0) | (1u64 << card_2), Ordering::Release);

  // Run a minor GC that scans the array via the remembered set.
  let mut remembered = SimpleRememberedSet::new();
  remembered.remember(arr_old);
  heap
    .collect_minor(&mut RootStack::new(), &mut remembered)
    .expect("minor GC");

  // Slots in marked cards should be updated out of the nursery.
  let updated_10 = unsafe {
    let data = arr_old.add(array::RT_ARRAY_DATA_OFFSET) as *mut *mut u8;
    *data.add(10)
  };
  assert!(!heap.is_in_nursery(updated_10));
  assert!(heap.is_in_immix(updated_10));

  let updated_126 = unsafe {
    let data = arr_old.add(array::RT_ARRAY_DATA_OFFSET) as *mut *mut u8;
    *data.add(126)
  };
  assert!(!heap.is_in_nursery(updated_126));
  assert!(heap.is_in_immix(updated_126));

  // Slot in the unmarked card should remain in the nursery.
  let still_young_70 = unsafe {
    let data = arr_old.add(array::RT_ARRAY_DATA_OFFSET) as *mut *mut u8;
    *data.add(70)
  };
  assert!(heap.is_in_nursery(still_young_70));

  // All card bits should be cleared after scanning.
  assert_eq!(card_table.word(0).load(Ordering::Acquire), 0);
}
