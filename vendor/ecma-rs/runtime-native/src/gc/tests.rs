use super::heap::GcHeap;
use super::roots::RememberedSet;
use super::roots::RootSet;
use super::roots::SimpleRememberedSet;
use super::TypeDescriptor;
use super::OBJ_HEADER_SIZE;
use crate::gc;
use crate::gc::RootStack;

const OBJ_SIZE: usize = 64;

static DESC_NO_PTR: TypeDescriptor = TypeDescriptor::new(OBJ_SIZE, &[]);

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

  heap.collect_major(&mut roots, &mut remembered);

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
    heap.collect_major(&mut roots, &mut remembered);

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
  static DESC_ONE_PTR: TypeDescriptor = TypeDescriptor::new(OBJ_HEADER_SIZE + std::mem::size_of::<*mut u8>(), &PTR_OFFSETS);

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

  heap.collect_minor(&mut roots, &mut remembered);

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
  let mut table = HandleTable::<u8>::new();
  let mut value = Box::new(123u8);
  let ptr = NonNull::from(value.as_mut());

  let id = table.alloc(ptr);
  assert_eq!(table.get(id), Some(ptr));

  assert!(table.free(id));
  assert_eq!(table.get(id), None);
}

#[test]
fn stale_generation_detection() {
  let mut table = HandleTable::<u8>::new();

  let mut v1 = Box::new(1u8);
  let id1 = table.alloc(NonNull::from(v1.as_mut()));
  assert!(table.free(id1));

  let mut v2 = Box::new(2u8);
  let id2 = table.alloc(NonNull::from(v2.as_mut()));

  // The old handle must not "resurrect" a new allocation in the reused slot.
  assert_eq!(table.get(id1), None);
  assert_eq!(table.get(id2), Some(NonNull::from(v2.as_mut())));
}

#[test]
fn slot_reuse_changes_generation() {
  let mut table = HandleTable::<u8>::new();

  let mut v1 = Box::new(1u8);
  let id1 = table.alloc(NonNull::from(v1.as_mut()));
  assert!(table.free(id1));

  let mut v2 = Box::new(2u8);
  let id2 = table.alloc(NonNull::from(v2.as_mut()));

  assert_eq!(id2.index(), id1.index());
  assert_ne!(id2.generation(), id1.generation());
}

#[test]
fn relocation_update_changes_get_result() {
  let mut table = HandleTable::<u8>::new();

  let mut v1 = Box::new(1u8);
  let mut v2 = Box::new(2u8);
  let ptr1 = NonNull::from(v1.as_mut());
  let ptr2 = NonNull::from(v2.as_mut());

  let id = table.alloc(ptr1);
  assert_eq!(table.get(id), Some(ptr1));

  assert!(table.update(id, ptr2));
  assert_eq!(table.get(id), Some(ptr2));
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
