use super::align_up;
use super::heap::{GcHeap, MajorCompactionConfig};
use super::roots::{RememberedSet, RootSet, SimpleRememberedSet};
use super::ObjHeader;
use super::TypeDescriptor;
use super::OBJ_HEADER_SIZE;
use crate::gc;
use crate::gc::RootStack;
use std::process::Command;

const OBJ_SIZE: usize = 64;

static DESC_NO_PTR: TypeDescriptor = TypeDescriptor::new(OBJ_SIZE, &[]);
static DESC_LINE: TypeDescriptor = TypeDescriptor::new(gc::heap::IMMIX_LINE_SIZE, &[]);

#[test]
fn align_up_basic() {
  assert_eq!(align_up(0, 8), 0);
  assert_eq!(align_up(1, 8), 8);
  assert_eq!(align_up(8, 8), 8);
  assert_eq!(align_up(9, 8), 16);
}

#[test]
fn major_gc_reclaims_old_blocks_for_reuse() {
  let mut heap = GcHeap::new();
  let mut roots = RootStack::new();
  let mut remembered = EmptyRememberedSet;

  for _ in 0..10_000 {
    heap.alloc_old(&DESC_NO_PTR);
  }

  let blocks_before = heap.immix_block_count();
  assert!(blocks_before > 0);

  heap.collect_major(&mut roots, &mut remembered).unwrap();

  for _ in 0..10_000 {
    heap.alloc_old(&DESC_NO_PTR);
  }

  let blocks_after = heap.immix_block_count();
  assert_eq!(
    blocks_after, blocks_before,
    "allocator should reuse swept blocks instead of growing the heap"
  );
}

#[test]
fn major_compaction_reuses_holes_without_growing_the_heap() {
  let mut heap = GcHeap::new();
  *heap.major_compaction_config_mut() = MajorCompactionConfig {
    enabled: true,
    ..MajorCompactionConfig::default()
  };
  let mut remembered = EmptyRememberedSet;

  const DEST_LIVE: usize = 200;
  assert!(DEST_LIVE > 0 && DEST_LIVE < gc::heap::IMMIX_LINES_PER_BLOCK);

  // Destination block: keep many objects live so it is *not* selected as a
  // compaction candidate, but free the tail so it has a large contiguous hole.
  let mut root_slots: Vec<Box<*mut u8>> = Vec::new();
  for _ in 0..DEST_LIVE {
    let obj = heap.alloc_old(&DESC_LINE);
    root_slots.push(Box::new(obj));
  }
  for _ in 0..(gc::heap::IMMIX_LINES_PER_BLOCK - DEST_LIVE) {
    heap.alloc_old(&DESC_LINE);
  }

  // Candidate block: one live object and the rest garbage.
  let candidate = heap.alloc_old(&DESC_LINE);
  let candidate_before = candidate;
  root_slots.push(Box::new(candidate));
  for _ in 0..(gc::heap::IMMIX_LINES_PER_BLOCK - 1) {
    heap.alloc_old(&DESC_LINE);
  }

  let blocks_before = heap.immix_block_count();
  assert_eq!(blocks_before, 2, "test setup should use exactly 2 Immix blocks");

  let mut roots = VecRootSet::from_boxed_slots(&mut root_slots);
  heap.collect_major(&mut roots, &mut remembered).unwrap();

  let blocks_after = heap.immix_block_count();
  assert_eq!(
    blocks_after, blocks_before,
    "compaction should reuse existing holes instead of allocating new blocks"
  );

  let candidate_after = *root_slots.last().unwrap().as_ref();
  assert_ne!(
    candidate_after, candidate_before,
    "expected the candidate object to be evacuated"
  );
}

#[test]
fn major_compaction_does_not_reclaim_blocks_with_pinned_immix_objects() {
  let mut heap = GcHeap::new();
  *heap.major_compaction_config_mut() = MajorCompactionConfig {
    enabled: true,
    ..MajorCompactionConfig::default()
  };

  let mut roots = RootStack::new();
  let mut remembered = EmptyRememberedSet;

  let mut pinned = heap.alloc_old(&DESC_LINE);
  let pinned_before = pinned;
  unsafe {
    // Simulate an Immix object becoming pinned (future-proofing): pinned objects
    // must remain in place even under compaction.
    (&mut *(pinned as *mut ObjHeader)).set_pinned(true);
    // Write a payload word so we can detect accidental reuse/overwrite.
    *((pinned as *mut u8).add(OBJ_HEADER_SIZE) as *mut u64) = 0xC0FFEE;
  }

  roots.push(&mut pinned as *mut *mut u8);

  // Fill the rest of the block with garbage so the pinned object's block is a
  // sparse candidate.
  for _ in 0..(gc::heap::IMMIX_LINES_PER_BLOCK - 1) {
    heap.alloc_old(&DESC_LINE);
  }

  let block_id = heap
    .immix
    .block_id_for_ptr(pinned_before)
    .expect("pinned object should be in Immix");

  heap.collect_major(&mut roots, &mut remembered).unwrap();

  assert_eq!(pinned, pinned_before);
  assert_eq!(
    unsafe { *((pinned as *mut u8).add(OBJ_HEADER_SIZE) as *const u64) },
    0xC0FFEE
  );

  let metrics = heap.immix.block_metrics(block_id).expect("block metrics");
  let live_lines = gc::heap::IMMIX_LINES_PER_BLOCK - metrics.free_lines;
  assert!(
    live_lines > 0,
    "pinned object's Immix block was treated as fully free under compaction"
  );
}

#[test]
fn old_space_does_not_grow_unbounded_under_fragmentation() {
  let mut heap = GcHeap::new();
  let mut remembered = EmptyRememberedSet;

  const KEEP_ROOTS: usize = 512;
  const GARBAGE_PER_CYCLE: usize = 5_000;
  const CYCLES: usize = 30;
  const MAX_IMMIX_BYTES: usize = 8 * 1024 * 1024;

  // Stable root slots to keep a bounded live set.
  let mut root_slots: Vec<Box<*mut u8>> = Vec::new();

  for _ in 0..KEEP_ROOTS {
    let obj = heap.alloc_old(&DESC_NO_PTR);
    root_slots.push(Box::new(obj));
  }

  for cycle in 0..CYCLES {
    // Drop half the roots to create scattered holes.
    while root_slots.len() > KEEP_ROOTS / 2 {
      root_slots.pop();
    }

    // Refill roots to keep the live set bounded.
    while root_slots.len() < KEEP_ROOTS {
      let obj = heap.alloc_old(&DESC_NO_PTR);
      root_slots.push(Box::new(obj));
    }

    // Allocate garbage to stress fragmentation and reuse.
    for _ in 0..GARBAGE_PER_CYCLE {
      heap.alloc_old(&DESC_NO_PTR);
    }

    let mut roots = VecRootSet::from_boxed_slots(&mut root_slots);
    heap.collect_major(&mut roots, &mut remembered).unwrap();

    let immix_bytes = heap.immix_block_count() * gc::heap::IMMIX_BLOCK_SIZE;
    assert!(
      immix_bytes <= MAX_IMMIX_BYTES,
      "immix grew too much (cycle {cycle}): {immix_bytes} bytes ({} blocks)",
      heap.immix_block_count()
    );
  }
}

#[test]
fn minor_gc_promotes_young_reachable_from_remembered_old_object() {
  let mut heap = GcHeap::new();

  const PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
  static DESC_ONE_PTR: TypeDescriptor =
    TypeDescriptor::new(OBJ_HEADER_SIZE + core::mem::size_of::<*mut u8>(), &PTR_OFFSETS);

  let mut remembered = SimpleRememberedSet::new();

  // Root an old object with one pointer slot.
  let mut old = heap.alloc_old(&DESC_ONE_PTR);
  let old_slot: *mut *mut u8 = &mut old;

  // Allocate a young object and store it into the old object's slot.
  let young = heap.alloc_young(&DESC_NO_PTR);
  unsafe {
    let field = (old as *mut u8).add(OBJ_HEADER_SIZE) as *mut *mut u8;
    *field = young;
  }
  remembered.remember(old);

  let mut roots = RootStack::new();
  roots.push(old_slot);

  heap.collect_minor(&mut roots, &mut remembered).unwrap();

  // The old object's field should now point to the promoted copy.
  let promoted = unsafe { *((old as *mut u8).add(OBJ_HEADER_SIZE) as *const *mut u8) };
  assert!(!promoted.is_null());
  assert!(
    !heap.is_in_nursery(promoted),
    "expected promoted object, but pointer is still in nursery"
  );
  assert!(
    heap.is_in_immix(promoted) || heap.is_in_los(promoted),
    "expected promoted object in old generation"
  );
}

#[test]
fn simple_remembered_set_remember_is_idempotent() {
  let mut heap = GcHeap::new();
  let mut remembered = SimpleRememberedSet::new();

  let obj = heap.alloc_old(&DESC_NO_PTR);

  remembered.remember(obj);
  remembered.remember(obj);

  let mut count = 0usize;
  remembered.for_each_remembered_obj(&mut |_| count += 1);
  assert_eq!(count, 1);

  // Clearing resets the per-object header bit so the object can be remembered again.
  remembered.clear();
  assert!(!unsafe { (&*(obj as *const gc::ObjHeader)).is_remembered() });

  remembered.remember(obj);
  let mut count2 = 0usize;
  remembered.for_each_remembered_obj(&mut |_| count2 += 1);
  assert_eq!(count2, 1);
}

#[test]
fn set_remembered_idempotent_does_not_corrupt_forwarded_header() {
  let mut heap = GcHeap::new();

  // Mark a nursery object as forwarded, then ensure `set_remembered_idempotent` does not mutate the
  // tagged forwarding pointer stored in `meta`.
  let new_location = heap.alloc_old(&DESC_NO_PTR);
  let obj = heap.alloc_young(&DESC_NO_PTR);

  let header = unsafe { &mut *(obj as *mut gc::ObjHeader) };
  header.set_forwarding_ptr(new_location);
  assert!(header.is_forwarded());
  let before = header.forwarding_ptr();

  assert!(!header.set_remembered_idempotent());
  assert_eq!(header.forwarding_ptr(), before);
}

#[test]
fn gc_ignores_non_heap_pointer_in_traced_slot() {
  const PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
  static DESC_ONE_PTR: TypeDescriptor =
    TypeDescriptor::new(OBJ_HEADER_SIZE + core::mem::size_of::<*mut u8>(), &PTR_OFFSETS);

  let mut heap = GcHeap::new();
  let mut remembered = EmptyRememberedSet;

  let obj = heap.alloc_old(&DESC_ONE_PTR);
  let external_ptr = Box::into_raw(Box::new(0u8)) as *mut u8;
  unsafe {
    let field = (obj as *mut u8).add(OBJ_HEADER_SIZE) as *mut *mut u8;
    *field = external_ptr;
  }

  let mut root = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);

  heap.collect_major(&mut roots, &mut remembered).unwrap();

  let stored = unsafe { *((root as *mut u8).add(OBJ_HEADER_SIZE) as *const *mut u8) };
  assert_eq!(stored, external_ptr);

  unsafe {
    drop(Box::from_raw(external_ptr));
  }
}

#[derive(Default)]
struct VecRootSet {
  slots: Vec<*mut *mut u8>,
}

impl VecRootSet {
  fn from_boxed_slots(boxes: &mut [Box<*mut u8>]) -> Self {
    let slots = boxes.iter_mut().map(|b| &mut **b as *mut *mut u8).collect();
    Self { slots }
  }
}

impl RootSet for VecRootSet {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for &slot in &self.slots {
      f(slot);
    }
  }
}

struct EmptyRememberedSet;

impl RememberedSet for EmptyRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}

  fn clear(&mut self) {}

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

use super::handle_table::{HandleId, HandleTable};
use core::ptr::NonNull;

#[test]
fn alloc_get_free_lifecycle() {
  let table = HandleTable::<u8>::new();

  let ptr = NonNull::new(Box::into_raw(Box::new(123u8))).unwrap();
  let id = table.alloc(ptr);

  assert_eq!(table.get(id), Some(ptr));

  let freed = table.free(id).expect("handle must be live");
  assert_eq!(freed, ptr);
  assert_eq!(table.get(id), None);

  unsafe {
    drop(Box::from_raw(ptr.as_ptr()));
  }
}

#[test]
fn stale_generation_detection() {
  let table = HandleTable::<u8>::new();

  let p1 = NonNull::new(Box::into_raw(Box::new(1u8))).unwrap();
  let id1 = table.alloc(p1);
  assert!(table.free(id1).is_some());
  unsafe {
    drop(Box::from_raw(p1.as_ptr()));
  }

  let p2 = NonNull::new(Box::into_raw(Box::new(2u8))).unwrap();
  let id2 = table.alloc(p2);

  // The old handle must not "resurrect" a new allocation in the reused slot.
  assert_eq!(table.get(id1), None);
  assert_eq!(table.get(id2), Some(p2));

  assert_eq!(id1.index(), id2.index());
  assert_ne!(id1.generation(), id2.generation());

  let freed = table.free(id2).unwrap();
  assert_eq!(freed, p2);
  unsafe {
    drop(Box::from_raw(p2.as_ptr()));
  }
}

#[test]
fn relocation_update_changes_get_result() {
  let table = HandleTable::<u8>::new();

  let p1 = NonNull::new(Box::into_raw(Box::new(1u8))).unwrap();
  let p2 = NonNull::new(Box::into_raw(Box::new(2u8))).unwrap();
  let id = table.alloc(p1);

  table.with_stw_update(|stw| {
    for (hid, slot) in stw.iter_live_mut() {
      if hid == id {
        *slot = p2.as_ptr();
      }
    }
  });

  assert_eq!(table.get(id), Some(p2));

  let freed = table.free(id).unwrap();
  assert_eq!(freed, p2);
  unsafe {
    drop(Box::from_raw(p1.as_ptr()));
    drop(Box::from_raw(p2.as_ptr()));
  }
}

#[test]
fn handle_id_round_trip_u64() {
  let id = HandleId::from_parts(123, 456);
  let raw: u64 = id.into();
  let id2 = HandleId::from(raw);

  assert_eq!(id, id2);
  assert_eq!(id.index(), 123);
  assert_eq!(id.generation(), 456);
}

use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn handle_table_is_send_sync_even_when_t_isnt() {
  fn assert_send_sync<T: Send + Sync>() {}

  // Rc is !Send + !Sync; HandleTable stores opaque pointers and should still be Send + Sync.
  assert_send_sync::<HandleTable<std::rc::Rc<()>>>();
}

#[test]
fn concurrent_get_with_alloc_free() {
  const READERS: usize = 4;
  const READ_ITERS: usize = 25_000;
  const WRITES: usize = 2_000;

  let table = Arc::new(HandleTable::<u8>::new());

  // A stable handle that readers will query while the writer thread mutates the table.
  let stable_box = Box::new(123u8);
  let stable_ptr = NonNull::new(Box::into_raw(stable_box)).unwrap();
  let stable_addr = stable_ptr.as_ptr() as usize;
  let stable_handle = table.alloc(stable_ptr);

  let start = Arc::new(Barrier::new(READERS + 1));

  let mut readers = Vec::new();
  for _ in 0..READERS {
    let table = Arc::clone(&table);
    let start = Arc::clone(&start);
    readers.push(thread::spawn(move || {
      start.wait();
      for _ in 0..READ_ITERS {
        let got = table.get(stable_handle).expect("stable handle must stay live");
        assert_eq!(got.as_ptr() as usize, stable_addr);
      }
    }));
  }

  let writer_table = Arc::clone(&table);
  let start_writer = Arc::clone(&start);
  let writer = thread::spawn(move || {
    start_writer.wait();
    for i in 0..WRITES {
      let ptr = NonNull::new(Box::into_raw(Box::new(i as u8))).unwrap();
      let h = writer_table.alloc(ptr);

      assert_eq!(writer_table.get(h).unwrap().as_ptr(), ptr.as_ptr());

      let freed = writer_table.free(h).expect("freshly allocated handle must be live");
      assert_eq!(freed.as_ptr(), ptr.as_ptr());

      // Safety: we just removed this pointer from the table.
      unsafe {
        drop(Box::from_raw(ptr.as_ptr()));
      }
    }
  });

  writer.join().unwrap();
  for reader in readers {
    reader.join().unwrap();
  }

  let freed = table.free(stable_handle).unwrap();
  assert_eq!(freed.as_ptr(), stable_ptr.as_ptr());
  unsafe {
    drop(Box::from_raw(stable_ptr.as_ptr()));
  }
}

#[test]
fn stw_relocation_updates_pointers() {
  let table = Arc::new(HandleTable::<u8>::new());

  let old_ptr = NonNull::new(Box::into_raw(Box::new(1u8))).unwrap();
  let new_ptr = NonNull::new(Box::into_raw(Box::new(2u8))).unwrap();

  let handle = table.alloc(old_ptr);

  let reader_table = Arc::clone(&table);
  let old_addr = old_ptr.as_ptr() as usize;
  let new_addr = new_ptr.as_ptr() as usize;
  let (tx_read, rx_read) = std::sync::mpsc::channel::<usize>();
  let (tx_updated, rx_updated) = std::sync::mpsc::channel::<()>();
  let reader = thread::spawn(move || {
    let before = reader_table.get(handle).unwrap().as_ptr();
    assert_eq!(before as usize, old_addr);
    tx_read.send(before as usize).unwrap();

    rx_updated.recv().unwrap();
    let after = reader_table.get(handle).unwrap().as_ptr();
    assert_eq!(after as usize, new_addr);
  });

  assert_eq!(rx_read.recv().unwrap(), old_addr);

  table.with_stw_update(|stw| {
    for slot in stw.iter_live_slots_mut() {
      if *slot == old_ptr.as_ptr() {
        *slot = new_ptr.as_ptr();
      }
    }
  });

  tx_updated.send(()).unwrap();
  reader.join().unwrap();

  // Cleanup.
  let freed = table.free(handle).unwrap();
  assert_eq!(freed.as_ptr(), new_ptr.as_ptr());
  unsafe {
    drop(Box::from_raw(old_ptr.as_ptr()));
    drop(Box::from_raw(new_ptr.as_ptr()));
  }
}

#[test]
fn align_up_overflow_child() {
  if std::env::var_os("GC_ALIGN_UP_OVERFLOW_CHILD").is_none() {
    return;
  }

  // This should overflow when rounding up to the next 8-byte boundary.
  let _ = align_up(usize::MAX - 3, 8);
  panic!("align_up should have trapped on overflow");
}

#[test]
fn align_up_overflow_traps() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("GC_ALIGN_UP_OVERFLOW_CHILD", "1")
    .arg("--exact")
    .arg("gc::tests::align_up_overflow_child")
    .status()
    .expect("spawn child");

  assert!(!status.success(), "expected child to abort/panic");
}
