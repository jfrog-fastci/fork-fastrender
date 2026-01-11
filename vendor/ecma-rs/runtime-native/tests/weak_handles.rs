use std::mem;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;

static NO_PTR_OFFSETS: [usize; 0] = [];

#[repr(C)]
struct BoxedUsize {
  header: ObjHeader,
  value: usize,
}

static BOXED_USIZE_DESC: TypeDescriptor =
  TypeDescriptor::new(mem::size_of::<BoxedUsize>(), &NO_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn weak_handle_clears_on_minor_gc_when_unreachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut roots = RootStack::new();
  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(heap.weak_get(handle), None);
}

#[test]
fn weak_handle_updates_on_minor_gc_when_reachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut root_obj = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root_obj as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());

  assert!(!heap.is_in_nursery(root_obj));
  assert_ne!(root_obj, obj);
  assert_eq!(heap.weak_get(handle), Some(root_obj));
}

#[test]
fn weak_handle_clears_on_major_gc_when_unreachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut roots = RootStack::new();
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(heap.weak_get(handle), None);
}

#[test]
fn weak_handle_survives_major_gc_when_reachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut root_obj = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root_obj as *mut *mut u8);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(root_obj, obj);
  assert_eq!(heap.weak_get(handle), Some(obj));
}

#[test]
fn weak_handle_remove_reuses_slots_with_generation_bumps() {
  let mut heap = GcHeap::new();

  let a = heap.alloc_old(&BOXED_USIZE_DESC);
  let ha = heap.weak_add(a);
  heap.weak_remove(ha);

  let b = heap.alloc_old(&BOXED_USIZE_DESC);
  let hb = heap.weak_add(b);

  assert_ne!(ha, hb);
  assert_eq!(heap.weak_get(ha), None);
  assert_eq!(heap.weak_get(hb), Some(b));
}
