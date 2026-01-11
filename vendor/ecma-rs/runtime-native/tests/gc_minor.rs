use std::mem;
use std::ptr;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
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

#[test]
fn minor_evacuation_updates_root_and_interior_pointers() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let a = heap.alloc_young(&NODE_DESC);
  let b = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(a as *mut Node)).next = b;
    (*(b as *mut Node)).next = ptr::null_mut();
  }

  let mut root_a = a;
  let mut root_b = b;
  let mut roots = RootStack::new();
  roots.push(&mut root_a as *mut *mut u8);
  roots.push(&mut root_b as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());

  assert!(!heap.is_in_nursery(root_a));
  assert!(!heap.is_in_nursery(root_b));
  assert!(heap.is_in_immix(root_a));
  assert!(heap.is_in_immix(root_b));

  let a_next = unsafe { (*(root_a as *mut Node)).next };
  assert_eq!(a_next, root_b, "forwarding must preserve object identity");
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
fn minor_gc_traces_remembered_old_objects() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let old = heap.alloc_old(&NODE_DESC);
  let young = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(old as *mut Node)).next = young;
  }

  let mut root_old = old;
  let mut roots = RootStack::new();
  roots.push(&mut root_old as *mut *mut u8);

  let mut remembered = VecRememberedSet::new(vec![old]);
  heap.collect_minor(&mut roots, &mut remembered);

  let updated = unsafe { (*(old as *mut Node)).next };
  assert!(!heap.is_in_nursery(updated));
  assert!(heap.is_in_immix(updated));
  assert!(remembered.cleared);
  assert!(remembered.objs.is_empty());
}
