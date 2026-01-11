use std::mem;
use std::ptr;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [usize; 1] = [mem::offset_of!(Node, next)];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[test]
fn promotion_registers_old_to_young_edge() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let parent_young = heap.alloc_young(&NODE_DESC);
  let child_young = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(parent_young as *mut Node)).next = child_young;
  }

  let parent_old = heap.alloc_old(&NODE_DESC);
  unsafe {
    ptr::copy_nonoverlapping(parent_young, parent_old, NODE_DESC.size);
  }

  assert!(!heap.is_in_nursery(parent_old));
  assert!(heap.is_in_nursery(child_young));

  let has_young_refs = unsafe { heap.is_in_nursery((*(parent_old as *mut Node)).next) };
  assert!(has_young_refs);

  let mut remembered = SimpleRememberedSet::new();
  remembered.on_promoted_object(parent_old, has_young_refs);

  assert!(remembered.contains(parent_old));
  assert!(unsafe { &*(parent_old as *const ObjHeader) }.is_remembered());

  remembered.scan_and_rebuild(|obj| unsafe { heap.is_in_nursery((*(obj as *const Node)).next) });
  assert!(remembered.contains(parent_old));
  assert!(unsafe { &*(parent_old as *const ObjHeader) }.is_remembered());

  unsafe {
    (*(parent_old as *mut Node)).next = ptr::null_mut();
  }
  remembered.scan_and_rebuild(|obj| unsafe { heap.is_in_nursery((*(obj as *const Node)).next) });
  assert!(!remembered.contains(parent_old));
  assert!(!unsafe { &*(parent_old as *const ObjHeader) }.is_remembered());
}
