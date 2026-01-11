use std::mem;

use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn young_evacuation_preserves_descriptor_alignment() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  static DESC: TypeDescriptor = TypeDescriptor::new_aligned(mem::size_of::<runtime_native::gc::ObjHeader>(), 32, &[]);

  let obj = heap.alloc_young(&DESC);
  assert_eq!((obj as usize) & 31, 0);

  let mut root = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);

  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(!heap.is_in_nursery(root));
  assert_eq!((root as usize) & 31, 0);
}

#[test]
fn pinned_allocation_honors_large_alignment() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  const ALIGN: usize = 1 << 20; // 1 MiB
  static DESC: TypeDescriptor =
    TypeDescriptor::new_aligned(mem::size_of::<runtime_native::gc::ObjHeader>(), ALIGN, &[]);

  // Allocate a few times to increase confidence we aren't relying on lucky `mmap`
  // base alignment.
  for _ in 0..8 {
    let obj = heap.alloc_pinned(&DESC);
    assert_eq!((obj as usize) % ALIGN, 0);
    assert!(heap.is_in_los(obj));
  }
}
