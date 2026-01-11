use std::mem;
use std::ptr;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [usize; 1] = [mem::offset_of!(Node, next)];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

struct VecRememberedSet {
  objs: Vec<*mut u8>,
  cleared: bool,
}

impl VecRememberedSet {
  fn new(objs: Vec<*mut u8>) -> Self {
    Self { objs, cleared: false }
  }
}

impl RememberedSet for VecRememberedSet {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    self.cleared = true;
    self.objs.clear();
  }

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn pinned_object_address_is_stable_across_minor_and_major_gc() {
  let mut heap = GcHeap::new();

  let pinned = heap.alloc_pinned(&NODE_DESC);
  assert!(heap.is_in_los(pinned), "pinned objects must live in LOS");

  let pinned_addr = pinned;
  let mut root_pinned = pinned;
  let mut roots = RootStack::new();
  roots.push(&mut root_pinned as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(root_pinned, pinned_addr);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(root_pinned, pinned_addr);
}

#[test]
fn pinned_objects_are_traced_and_compat_with_minor_evacuation() {
  let mut heap = GcHeap::new();

  let pinned = heap.alloc_pinned(&NODE_DESC);
  let young = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(pinned as *mut Node)).next = young;
    (*(young as *mut Node)).next = ptr::null_mut();
  }

  let mut root_pinned = pinned;
  let mut roots = RootStack::new();
  roots.push(&mut root_pinned as *mut *mut u8);

  // The pinned object now contains an old->young edge, which would normally be recorded by the
  // write barrier. For the test, we provide it explicitly via a remembered set.
  let mut remembered = VecRememberedSet::new(vec![pinned]);
  heap.collect_minor(&mut roots, &mut remembered);

  assert_eq!(root_pinned, pinned);
  let updated = unsafe { (*(pinned as *mut Node)).next };
  assert_ne!(updated, young);
  assert!(!heap.is_in_nursery(updated));
  assert!(heap.is_in_immix(updated));
  assert!(remembered.cleared);
  assert!(remembered.objs.is_empty());

  // Major GC should keep both pinned + its child alive.
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(unsafe { (*(pinned as *mut Node)).next }, updated);
}

#[test]
fn unreachable_pinned_objects_are_collectible() {
  let mut heap = GcHeap::new();
  assert_eq!(heap.los_object_count(), 0);

  let _pinned = heap.alloc_pinned(&NODE_DESC);
  assert_eq!(heap.los_object_count(), 1);

  let mut roots = RootStack::new();
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(heap.los_object_count(), 0);
}
