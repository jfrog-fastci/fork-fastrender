use std::mem;

use runtime_native::gc::heap::IMMIX_LINE_SIZE;
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;

static NO_PTR_OFFSETS: [usize; 0] = [];

#[repr(C)]
struct LineObject {
  header: ObjHeader,
  bytes: [u8; IMMIX_LINE_SIZE - mem::size_of::<ObjHeader>()],
}

static LINE_OBJECT_DESC: TypeDescriptor =
  TypeDescriptor::new(mem::size_of::<LineObject>(), &NO_PTR_OFFSETS);

const BIG_OBJECT_SIZE: usize = IMMIX_MAX_OBJECT_SIZE + 64;
static BIG_OBJECT_DESC: TypeDescriptor = TypeDescriptor::new(BIG_OBJECT_SIZE, &NO_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
}

#[test]
fn major_gc_reclaims_dead_immix_lines_and_sweeps_large_objects() {
  assert_eq!(mem::size_of::<LineObject>(), IMMIX_LINE_SIZE);

  let mut heap = GcHeap::new();

  let dead = heap.alloc_old(&LINE_OBJECT_DESC);
  let live = heap.alloc_old(&LINE_OBJECT_DESC);

  let big = heap.alloc_old(&BIG_OBJECT_DESC);
  assert!(heap.is_in_los(big));
  assert_eq!(heap.los_object_count(), 1);

  let mut root_live = live;
  let mut roots = RootStack::new();
  roots.push(&mut root_live as *mut *mut u8);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(heap.los_object_count(), 0);

  // Allocate again: should reuse the reclaimed line where `dead` used to live.
  let reused = heap.alloc_old(&LINE_OBJECT_DESC);
  assert_eq!(reused, dead);
  assert_eq!(root_live, live);
}

