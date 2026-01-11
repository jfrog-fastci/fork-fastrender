use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use runtime_native::gc::{ObjHeader, TypeDescriptor, CARD_SIZE};
use runtime_native::test_util::TestRuntimeGuard;

const THREADS: usize = 8;
const SLOTS_PER_THREAD: usize = 256;
const TOTAL_SLOTS: usize = THREADS * SLOTS_PER_THREAD;

const ITERS_PER_THREAD: usize = 50_000;

#[repr(C)]
struct CardArray {
  header: ObjHeader,
  slots: [*mut u8; TOTAL_SLOTS],
}

static CARD_ARRAY_DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<CardArray>(), &[]);

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

fn xorshift64(mut x: u64) -> impl FnMut() -> u64 {
  move || {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
  }
}

#[test]
fn concurrent_write_barrier_is_data_race_free_for_metadata() {
  let mut young_mem: Box<[u8]> = vec![0u8; 1024 * 1024].into_boxed_slice();
  let young_ptr = young_mem.as_mut_ptr();
  let young_end = unsafe { young_ptr.add(young_mem.len()) };

  let mut obj = Box::new(CardArray {
    header: ObjHeader::new(&CARD_ARRAY_DESC),
    slots: [core::ptr::null_mut(); TOTAL_SLOTS],
  });

  let obj_size = core::mem::size_of::<CardArray>();
  let card_count = obj_size.div_ceil(CARD_SIZE);
  assert!(
    card_count <= 64,
    "test assumes <= 64 cards but got {card_count} (obj_size={obj_size})"
  );
  let word_count = card_count.div_ceil(64);
  let cards = AlignedCardTable::new(word_count);
  unsafe {
    obj.header.set_card_table_ptr(cards.ptr);
  }

  // Hold the global runtime lock while calling into exported runtime functions,
  // but ensure it drops *before* `obj` so reset doesn't dereference freed objects.
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_gc_set_young_range(young_ptr, young_end);

  // Rust (as of 1.92) does not implement `Send` for raw pointers, so pass them to worker threads
  // as plain addresses.
  let young_ptr_addr = young_ptr as usize;
  let obj_ptr_addr = obj.as_mut() as *mut CardArray as usize;
  let slots_base_addr = obj_ptr_addr + core::mem::offset_of!(CardArray, slots);

  let expected = Arc::new(AtomicU64::new(0));
  let start = Arc::new(Barrier::new(THREADS));

  let mut handles = Vec::new();
  for tid in 0..THREADS {
    let expected = Arc::clone(&expected);
    let start = Arc::clone(&start);

    handles.push(thread::spawn(move || {
      let obj_ptr = obj_ptr_addr as *mut u8;
      let slots_base = slots_base_addr as *mut *mut u8;
      let young_ptr = young_ptr_addr as *mut u8;

      let mut rng = xorshift64((tid as u64) + 1);
      start.wait();

      // Writes are confined to this thread's own strided slot partition (`idx % THREADS == tid`),
      // so the test does not introduce data races on the object fields themselves (only on the
      // shared metadata that the barrier updates).
      for _ in 0..ITERS_PER_THREAD {
        let k = (rng() as usize) % SLOTS_PER_THREAD;
        let idx = tid + k * THREADS;
        debug_assert!(idx < TOTAL_SLOTS);
        let slot_ptr = unsafe { slots_base.add(idx) };

        unsafe {
          slot_ptr.write(young_ptr);
          runtime_native::rt_write_barrier(obj_ptr, slot_ptr.cast::<u8>());
        }

        let slot_offset = (slot_ptr as usize).wrapping_sub(obj_ptr as usize);
        let card_idx = slot_offset / CARD_SIZE;
        expected.fetch_or(1u64 << card_idx, Ordering::Relaxed);
      }
    }));
  }

  for h in handles {
    h.join().expect("thread panicked");
  }

  // Remembered-set insertion should be idempotent (one object, inserted once).
  assert!(obj.header.is_remembered());
  assert!(runtime_native::remembered_set_contains(obj_ptr_addr as *mut u8));
  assert_eq!(runtime_native::remembered_set_len_for_tests(), 1);

  // Card marks should include all cards that were touched by stores.
  let expected_bits = expected.load(Ordering::Relaxed);
  let observed_bits = cards.word(0);
  assert_eq!(
    observed_bits & expected_bits,
    expected_bits,
    "card table lost dirty marks (observed={observed_bits:#x}, expected={expected_bits:#x})"
  );
}
