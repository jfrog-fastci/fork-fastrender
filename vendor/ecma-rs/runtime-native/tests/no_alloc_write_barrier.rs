use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use runtime_native::gc::{ObjHeader, TypeDescriptor, CARD_SIZE, OBJ_HEADER_SIZE};
use runtime_native::test_util::TestGcGuard;
use runtime_native::GcHeap;

struct CountingAlloc;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }
}

#[repr(align(16))]
struct AlignedCardTable([AtomicU64; 1]);

#[test]
fn write_barriers_do_not_allocate() {
  let _gc = TestGcGuard::new();

  // Allocate test objects and a young-range sentinel *before* resetting the allocation counter.
  let mut young_byte = Box::new(0u8);
  let young_ptr = (&mut *young_byte) as *mut u8;
  unsafe {
    runtime_native::rt_gc_set_young_range(young_ptr, young_ptr.add(1));
  }

  // Use a real GC heap allocation so `ObjHeader::type_desc` is non-null and
  // `rt_write_barrier_range` can take its card-table slow path.
  static PTR_OFFSETS: [u32; 0] = [];
  // Ensure the object has at least one pointer-sized slot after the header.
  const OBJ_SIZE: usize = CARD_SIZE;
  static DESC: TypeDescriptor = TypeDescriptor::new(OBJ_SIZE, &PTR_OFFSETS);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_old(&DESC);
  let header = unsafe { &mut *(obj as *mut ObjHeader) };

  // Install a one-word per-object card table (enough for any object <= 64 cards).
  let mut cards = AlignedCardTable([AtomicU64::new(0)]);
  unsafe {
    header.set_card_table_ptr(cards.0.as_mut_ptr());
  }

  // Store a young pointer into a slot within the object and invoke the per-slot barrier.
  let slot_ptr = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut *mut u8 };
  unsafe {
    slot_ptr.write(young_ptr);
  }

  // Reset after all setup work so the measured section only covers the exported barrier calls.
  ALLOC_CALLS.store(0, Ordering::SeqCst);

  unsafe {
    runtime_native::rt_write_barrier(obj, slot_ptr.cast::<u8>());
    runtime_native::rt_write_barrier_range(obj, slot_ptr.cast::<u8>(), mem::size_of::<*mut u8>());
  }

  let allocs = ALLOC_CALLS.load(Ordering::SeqCst);
  assert!(header.is_remembered());
  assert_ne!(cards.0[0].load(Ordering::Acquire), 0);
  assert_eq!(
    allocs, 0,
    "write barrier performed unexpected allocations (alloc calls={allocs})"
  );
}
