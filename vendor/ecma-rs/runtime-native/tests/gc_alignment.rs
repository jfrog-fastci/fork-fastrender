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

#[test]
fn major_compaction_preserves_descriptor_alignment() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();
  heap.major_compaction_config_mut().enabled = true;
  // Treat any non-empty Immix block with <100% live lines as a compaction candidate so this test
  // doesn't depend on a particular occupancy ratio.
  heap.major_compaction_config_mut().max_live_ratio_percent = 100;

  static DESC_16: TypeDescriptor = TypeDescriptor::new_aligned(mem::size_of::<runtime_native::gc::ObjHeader>(), 16, &[]);
  static DESC_32: TypeDescriptor = TypeDescriptor::new_aligned(mem::size_of::<runtime_native::gc::ObjHeader>(), 32, &[]);

  // Allocate a 16-byte-aligned object first so the compactor's to-space cursor is offset by 16
  // bytes. Without honoring `TypeDescriptor::align`, the subsequent 32-byte-aligned object could be
  // relocated to an address that is only 16-byte aligned.
  let obj16 = heap.alloc_old(&DESC_16);
  let obj32 = heap.alloc_old(&DESC_32);
  assert!(heap.is_in_immix(obj16));
  assert!(heap.is_in_immix(obj32));
  assert_eq!((obj32 as usize) & 31, 0);

  let mut root16 = obj16;
  let mut root32 = obj32;
  let mut roots = RootStack::new();
  roots.push(&mut root16 as *mut *mut u8);
  roots.push(&mut root32 as *mut *mut u8);

  heap
    .collect_major(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert_ne!(root32, obj32, "expected major compaction to relocate the object");
  assert!(!heap.is_in_nursery(root32));
  assert_eq!(
    (root32 as usize) & 31,
    0,
    "compaction did not preserve 32-byte descriptor alignment"
  );
}
