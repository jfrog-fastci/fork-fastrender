use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use runtime_native::gc::{ObjHeader, TypeDescriptor, CARD_SIZE};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;

struct AlignedCardTable {
  ptr: *mut AtomicU64,
  layout: Layout,
  word_count: usize,
}

impl AlignedCardTable {
  fn new(word_count: usize) -> Self {
    assert!(word_count > 0);
    let bytes = word_count * core::mem::size_of::<AtomicU64>();
    let layout = Layout::from_size_align(bytes, 16).expect("invalid card table layout");
    let ptr = unsafe { alloc_zeroed(layout) }.cast::<AtomicU64>();
    assert!(!ptr.is_null());
    Self {
      ptr,
      layout,
      word_count,
    }
  }

  fn word(&self, idx: usize) -> u64 {
    assert!(idx < self.word_count);
    unsafe { (*self.ptr.add(idx)).load(Ordering::Acquire) }
  }
}

impl Drop for AlignedCardTable {
  fn drop(&mut self) {
    unsafe { dealloc(self.ptr.cast::<u8>(), self.layout) }
  }
}

#[test]
fn write_barrier_range_marks_all_covered_cards_and_remembers_object() {
  const TOTAL_SIZE: usize = CARD_SIZE * 4;
  static PTR_OFFSETS: [u32; 0] = [];
  static DESC: TypeDescriptor = TypeDescriptor::new(TOTAL_SIZE, &PTR_OFFSETS);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_old(&DESC);
  let header = unsafe { &mut *(obj as *mut ObjHeader) };

  let card_count = TOTAL_SIZE.div_ceil(CARD_SIZE);
  let word_count = card_count.div_ceil(64);
  let cards = AlignedCardTable::new(word_count);
  unsafe {
    header.set_card_table_ptr(cards.ptr);
  }

  // Hold the global runtime lock while calling into exported runtime functions,
  // but ensure it drops *before* `heap` so reset doesn't dereference freed objects.
  let _rt = TestRuntimeGuard::new();

  // Treat all objects as old for this test.
  runtime_native::rt_gc_set_young_range(core::ptr::null_mut(), core::ptr::null_mut());

  // Pick a range that starts near the end of card 0 and runs into card 2.
  let start_offset = CARD_SIZE - 8;
  let len = CARD_SIZE + 16;
  let start_ptr = unsafe { obj.add(start_offset) };

  unsafe {
    runtime_native::rt_write_barrier_range(obj, start_ptr, len);
  }

  // Covered cards: 0, 1, 2. (bitset uses one bit per card).
  assert_eq!(cards.word(0) & 0b111, 0b111);
  assert_eq!(cards.word(0) & (1u64 << 3), 0);

  // Must be remembered (header flag).
  assert!(header.is_remembered());
}

#[test]
fn write_barrier_range_fast_paths_young_object_and_zero_len() {
  const TOTAL_SIZE: usize = CARD_SIZE * 2;
  const PAYLOAD_BYTES: usize = TOTAL_SIZE - mem::size_of::<ObjHeader>();

  #[repr(C)]
  struct FakeObj {
    header: ObjHeader,
    payload: [u8; PAYLOAD_BYTES],
  }

  // Young object: should be a no-op (the barrier returns before touching the header/card table).
  let mut obj: Box<FakeObj> = unsafe { Box::new(mem::zeroed()) };
  let obj_ptr = obj.as_mut() as *mut FakeObj as *mut u8;

  let cards = AlignedCardTable::new(1);
  unsafe {
    obj.header.set_card_table_ptr(cards.ptr);
  }

  let mut obj2: Box<FakeObj> = unsafe { Box::new(mem::zeroed()) };
  let obj2_ptr = obj2.as_mut() as *mut FakeObj as *mut u8;
  let cards2 = AlignedCardTable::new(1);
  unsafe {
    obj2.header.set_card_table_ptr(cards2.ptr);
  }

  let _rt = TestRuntimeGuard::new();

  runtime_native::rt_gc_set_young_range(obj_ptr, unsafe { obj_ptr.add(TOTAL_SIZE) });

  let start_ptr = unsafe { obj_ptr.add(CARD_SIZE / 2) };
  unsafe {
    runtime_native::rt_write_barrier_range(obj_ptr, start_ptr, 16);
  }
  assert_eq!(cards.word(0), 0);
  assert!(!obj.header.is_remembered());

  // Old object but zero length: should be a no-op.
  runtime_native::rt_gc_set_young_range(core::ptr::null_mut(), core::ptr::null_mut());
  let start2_ptr = unsafe { obj2_ptr.add(CARD_SIZE / 2) };
  unsafe {
    runtime_native::rt_write_barrier_range(obj2_ptr, start2_ptr, 0);
  }
  assert_eq!(cards2.word(0), 0);
  assert!(!obj2.header.is_remembered());
}
