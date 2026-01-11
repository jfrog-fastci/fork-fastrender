use super::heap::GcHeap;
use super::roots::RememberedSet;
use super::roots::RootSet;
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

  let mut remembered = VecRememberedSet::default();

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

#[derive(Default)]
struct VecRememberedSet {
  objs: Vec<*mut u8>,
}

impl VecRememberedSet {
  fn remember(&mut self, obj: *mut u8) {
    self.objs.push(obj);
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

struct EmptyRememberedSet;

impl RememberedSet for EmptyRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}

  fn clear(&mut self) {}

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}
