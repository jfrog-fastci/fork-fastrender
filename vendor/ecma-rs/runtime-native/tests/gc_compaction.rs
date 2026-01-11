use std::mem;
use std::ptr;

use runtime_native::gc::heap::IMMIX_LINE_SIZE;
use runtime_native::gc::heap::IMMIX_LINES_PER_BLOCK;
use runtime_native::gc::heap::MajorCompactionConfig;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

static NO_PTR_OFFSETS: [u32; 0] = [];

#[repr(C)]
struct LeafLine {
  header: ObjHeader,
  value: usize,
  bytes: [u8; IMMIX_LINE_SIZE - mem::size_of::<ObjHeader>() - mem::size_of::<usize>()],
}

static LEAF_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<LeafLine>(), &NO_PTR_OFFSETS);

#[repr(C)]
struct NodeLine {
  header: ObjHeader,
  next: *mut u8,
  value: usize,
  bytes: [u8; IMMIX_LINE_SIZE
    - mem::size_of::<ObjHeader>()
    - mem::size_of::<*mut u8>()
    - mem::size_of::<usize>()],
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(NodeLine, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<NodeLine>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

fn alloc_dead_lines(heap: &mut GcHeap, count: usize) {
  for _ in 0..count {
    let _ = heap.alloc_old(&LEAF_DESC);
  }
}

#[test]
fn major_gc_compaction_evacuates_sparse_blocks_and_preserves_payloads() {
  let _rt = TestRuntimeGuard::new();
  assert_eq!(mem::size_of::<LeafLine>(), IMMIX_LINE_SIZE);
  assert_eq!(mem::size_of::<NodeLine>(), IMMIX_LINE_SIZE);

  let mut heap = GcHeap::new();
  *heap.major_compaction_config_mut() = MajorCompactionConfig {
    enabled: true,
    ..MajorCompactionConfig::default()
  };

  let mut roots = RootStack::new();

  // Pinned object: should not move under compaction.
  let pinned = heap.alloc_pinned(&LEAF_DESC);
  unsafe {
    (*(pinned as *mut LeafLine)).value = 0xC0FFEE;
  }
  let pinned_before = pinned;
  let mut root_pinned = pinned;
  roots.push(&mut root_pinned as *mut *mut u8);

  // Candidate block: a single live leaf.
  let first = heap.alloc_old(&LEAF_DESC);
  unsafe {
    (*(first as *mut LeafLine)).value = 42;
  }
  let mut root_first = first;
  roots.push(&mut root_first as *mut *mut u8);
  alloc_dead_lines(&mut heap, IMMIX_LINES_PER_BLOCK - 1);

  // Candidate block: a tiny object graph parent -> child.
  let child = heap.alloc_old(&LEAF_DESC);
  unsafe {
    (*(child as *mut LeafLine)).value = 0xBEEF;
  }

  let parent = heap.alloc_old(&NODE_DESC);
  unsafe {
    let parent_ref = &mut *(parent as *mut NodeLine);
    parent_ref.next = child;
    parent_ref.value = 0xD00D;
  }

  let parent_before = parent;
  let child_before = child;
  let mut root_parent = parent;
  roots.push(&mut root_parent as *mut *mut u8);

  alloc_dead_lines(&mut heap, IMMIX_LINES_PER_BLOCK - 2);

  // Additional sparse blocks to create fragmentation across multiple blocks.
  let mut live = [ptr::null_mut(); 3];
  for (i, slot) in live.iter_mut().enumerate() {
    *slot = heap.alloc_old(&LEAF_DESC);
    unsafe {
      (*(*slot).cast::<LeafLine>()).value = 1000 + i;
    }
    roots.push(slot as *mut *mut u8);
    alloc_dead_lines(&mut heap, IMMIX_LINES_PER_BLOCK - 1);
  }

  let free_before = heap.immix_free_block_count();
  let live_before = live;

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  let free_after = heap.immix_free_block_count();
  assert!(
    free_after > free_before,
    "expected more completely-free Immix blocks after compaction (before={free_before}, after={free_after})"
  );

  assert_eq!(root_pinned, pinned_before);
  assert_eq!(unsafe { (*(root_pinned as *mut LeafLine)).value }, 0xC0FFEE);

  assert_eq!(unsafe { (*(root_first as *mut LeafLine)).value }, 42);

  let moved_parent = root_parent != parent_before;
  let moved_any_leaf = live
    .iter()
    .zip(live_before.iter())
    .any(|(&after, &before)| after != before);
  assert!(
    moved_parent || moved_any_leaf || root_first != first,
    "expected at least one live object to move under compaction"
  );

  assert_eq!(unsafe { (*(root_parent as *mut NodeLine)).value }, 0xD00D);
  let child_after = unsafe { (*(root_parent as *mut NodeLine)).next };
  assert_ne!(child_after, child_before);
  assert_eq!(unsafe { (*(child_after as *mut LeafLine)).value }, 0xBEEF);

  for (i, &obj) in live.iter().enumerate() {
    assert_eq!(unsafe { (*(obj as *mut LeafLine)).value }, 1000 + i);
  }
}

#[test]
fn major_gc_without_compaction_does_not_move() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();
  assert!(!heap.major_compaction_config().enabled);

  let mut roots = RootStack::new();

  let obj = heap.alloc_old(&LEAF_DESC);
  unsafe {
    (*(obj as *mut LeafLine)).value = 123;
  }
  let obj_before = obj;
  let mut root_obj = obj;
  roots.push(&mut root_obj as *mut *mut u8);

  // Fill enough blocks to ensure we run a real major GC.
  alloc_dead_lines(&mut heap, IMMIX_LINES_PER_BLOCK * 2);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(root_obj, obj_before);
  assert_eq!(unsafe { (*(root_obj as *mut LeafLine)).value }, 123);
}
