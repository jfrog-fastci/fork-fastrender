use std::mem;

use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;

static NO_PTR_OFFSETS: [usize; 0] = [];

#[repr(C)]
struct Blob {
  header: ObjHeader,
  value: u64,
}

static BLOB_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Blob>(), &NO_PTR_OFFSETS);

const BIG_OBJECT_SIZE: usize = IMMIX_MAX_OBJECT_SIZE + 64;
static BIG_OBJECT_DESC: TypeDescriptor = TypeDescriptor::new(BIG_OBJECT_SIZE, &NO_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn root_handle_survives_minor_gc_and_updates_pointer() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BLOB_DESC);
  unsafe {
    (*(obj as *mut Blob)).value = 123;
  }

  let h = heap.root_add(obj);

  let mut roots = RootStack::new();
  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());

  let updated = heap.root_get(h).unwrap();
  assert_ne!(updated, obj);
  assert!(!heap.is_in_nursery(updated));
  assert!(heap.is_in_immix(updated));
  unsafe {
    assert_eq!((updated as *const Blob).as_ref().unwrap().value, 123);
  }
}

#[test]
fn root_handle_keeps_object_alive_across_major_gc() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BIG_OBJECT_DESC);
  assert!(heap.is_in_los(obj));
  assert_eq!(heap.los_object_count(), 1);

  let h = heap.root_add(obj);

  let mut roots = RootStack::new();
  let mut remembered = NullRememberedSet::default();
  heap.collect_major(&mut roots, &mut remembered);

  assert_eq!(heap.los_object_count(), 1);
  assert_eq!(heap.root_get(h).unwrap(), obj);

  heap.root_remove(h);
  heap.collect_major(&mut roots, &mut remembered);
  assert_eq!(heap.los_object_count(), 0);
}

#[test]
fn stale_handle_detection() {
  let mut heap = GcHeap::new();

  let obj1 = heap.alloc_old(&BLOB_DESC);
  let h1 = heap.root_add(obj1);
  heap.root_remove(h1);

  let obj2 = heap.alloc_old(&BLOB_DESC);
  let h2 = heap.root_add(obj2);

  assert_eq!(h1.index(), h2.index());
  assert_ne!(h1.generation(), h2.generation());
  assert!(heap.root_get(h1).is_none());
  assert_eq!(heap.root_get(h2).unwrap(), obj2);

  // Stale handles must not affect new occupants of the same slot.
  heap.root_remove(h1);
  assert_eq!(heap.root_get(h2).unwrap(), obj2);
}

