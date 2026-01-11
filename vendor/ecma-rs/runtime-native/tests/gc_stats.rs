#![cfg(feature = "gc_stats")]

use std::mem;
use std::ptr;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::sync::atomic::AtomicU64;

use runtime_native::abi::RtGcStatsSnapshot;
use runtime_native::gc::{ObjHeader, RememberedSet, RootStack, TypeDescriptor, CARD_SIZE};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

struct AlignedCardTable {
  ptr: *mut AtomicU64,
  layout: Layout,
}

impl AlignedCardTable {
  fn new(word_count: usize) -> Self {
    assert!(word_count > 0);
    let bytes = word_count * core::mem::size_of::<AtomicU64>();
    let layout = Layout::from_size_align(bytes, 16).expect("invalid card table layout");
    let ptr = unsafe { alloc_zeroed(layout) }.cast::<AtomicU64>();
    assert!(!ptr.is_null());
    Self { ptr, layout }
  }
}

impl Drop for AlignedCardTable {
  fn drop(&mut self) {
    unsafe { dealloc(self.ptr.cast::<u8>(), self.layout) }
  }
}

fn snapshot() -> RtGcStatsSnapshot {
  let mut out = RtGcStatsSnapshot::default();
  unsafe {
    runtime_native::rt_gc_stats_snapshot(&mut out as *mut RtGcStatsSnapshot);
  }
  out
}

fn delta_u64(before: u64, after: u64) -> u64 {
  after.saturating_sub(before)
}

struct VecRememberedSet {
  objs: Vec<*mut u8>,
}

impl VecRememberedSet {
  fn new(objs: Vec<*mut u8>) -> Self {
    Self { objs }
  }
}

impl RememberedSet for VecRememberedSet {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    self.objs.clear();
  }

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn gc_stats_counters_increment_across_barrier_and_minor_scan() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_gc_stats_reset();
  let before = snapshot();

  let mut heap = GcHeap::new();
  runtime_native::test_util::gc_set_young_range_for_heap(&heap);

  // Old object with a single pointer slot (remembered-set path).
  let old = heap.alloc_old(&NODE_DESC);
  // Give it a 1-word card table so the scalar barrier records a card mark.
  let _old_cards = AlignedCardTable::new(1);
  unsafe {
    (*(old as *mut ObjHeader)).set_card_table_ptr(_old_cards.ptr);
  }

  let young = heap.alloc_young(&NODE_DESC);
  unsafe {
    (*(old as *mut Node)).next = young;
    let slot = ptr::addr_of_mut!((*(old as *mut Node)).next) as *mut u8;
    runtime_native::rt_write_barrier(old, slot);
  }

  // Old object with a larger card table hit via `rt_write_barrier_range`.
  const BIG_SIZE: usize = CARD_SIZE * 4;
  static BIG_PTR_OFFSETS: [u32; 0] = [];
  static BIG_DESC: TypeDescriptor = TypeDescriptor::new(BIG_SIZE, &BIG_PTR_OFFSETS);
  let big = heap.alloc_old(&BIG_DESC);
  let _big_cards = AlignedCardTable::new(1);
  unsafe {
    (*(big as *mut ObjHeader)).set_card_table_ptr(_big_cards.ptr);
  }
  unsafe {
    // Mark cards 0..=2.
    let start_offset = CARD_SIZE - 8;
    let len = CARD_SIZE + 16;
    runtime_native::rt_write_barrier_range(big, big.add(start_offset), len);
  }

  // Minor GC scans remembered objects. This test supplies the remembered set explicitly; the
  // exported write barrier uses `gc::global_remset` to feed the collector in the real runtime.
  let mut root_old = old;
  let mut roots = RootStack::new();
  roots.push(&mut root_old as *mut *mut u8);

  let mut remembered = VecRememberedSet::new(vec![old, big]);
  heap.collect_minor(&mut roots, &mut remembered).expect("minor GC");

  let after = snapshot();

  let d_write_barrier_calls =
    delta_u64(before.write_barrier_calls_total, after.write_barrier_calls_total);
  let d_old_young_hits = delta_u64(before.write_barrier_old_young_hits, after.write_barrier_old_young_hits);
  let d_remembered_added = delta_u64(before.remembered_objects_added, after.remembered_objects_added);
  let d_remembered_scanned_minor =
    delta_u64(before.remembered_objects_scanned_minor, after.remembered_objects_scanned_minor);
  let d_card_marks = delta_u64(before.card_marks_total, after.card_marks_total);
  let d_cards_scanned_minor = delta_u64(before.cards_scanned_minor, after.cards_scanned_minor);
  let d_cards_kept_after_rebuild =
    delta_u64(before.cards_kept_after_rebuild, after.cards_kept_after_rebuild);

  assert_eq!(d_write_barrier_calls, 1);
  assert_eq!(d_old_young_hits, 1);
  assert_eq!(d_remembered_added, 2);
  assert_eq!(d_remembered_scanned_minor, 2);
  // Scalar barrier marks 1 card; range barrier marks 3 cards.
  assert_eq!(d_card_marks, 4);
  assert_eq!(d_cards_scanned_minor, 4);
  assert_eq!(d_cards_kept_after_rebuild, 0);

  // Avoid leaking the heap's nursery range into other tests (young range is global).
  runtime_native::clear_write_barrier_state_for_tests();
}

#[test]
fn gc_stats_remembered_added_not_doubled_by_remset_drain() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_gc_stats_reset();
  let before = snapshot();

  // Use a registered thread so the write barrier records into a thread-local
  // remset buffer (drained via `gc::global_remset`).
  runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::External);

  let mut heap = GcHeap::new();
  runtime_native::test_util::gc_set_young_range_for_heap(&heap);

  let old = heap.alloc_old(&NODE_DESC);
  let young = heap.alloc_young(&NODE_DESC);
  unsafe {
    (*(old as *mut Node)).next = young;
    let slot = ptr::addr_of_mut!((*(old as *mut Node)).next) as *mut u8;
    runtime_native::rt_write_barrier(old, slot);
  }

  let after_barrier = snapshot();

  let mut remembered = runtime_native::gc::SimpleRememberedSet::new();
  runtime_native::gc::global_remset::remset_drain_into(&mut remembered);
  assert!(remembered.contains(old));

  let after_drain = snapshot();

  // Draining the write-barrier buffers into the GC's remembered set should not
  // double-count remembered-object additions: the barrier already recorded the
  // 0→1 transition when it set the `REMEMBERED` bit.
  let d_added_by_barrier =
    delta_u64(before.remembered_objects_added, after_barrier.remembered_objects_added);
  let d_added_by_drain =
    delta_u64(after_barrier.remembered_objects_added, after_drain.remembered_objects_added);
  assert_eq!(d_added_by_barrier, 1);
  assert_eq!(d_added_by_drain, 0);

  // Avoid leaking the heap's nursery range into other tests (young range is global).
  runtime_native::clear_write_barrier_state_for_tests();
}
