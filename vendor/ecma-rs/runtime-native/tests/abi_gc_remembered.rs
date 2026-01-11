use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  remembered_set_contains, rt_alloc, rt_gc_collect, rt_gc_get_young_range, rt_gc_register_root_slot, rt_gc_unregister_root_slot,
  rt_thread_deinit, rt_thread_init, rt_write_barrier, shape_table,
};

#[repr(C)]
struct Leaf {
  _header: runtime_native::gc::ObjHeader,
  value: usize,
}

#[repr(C)]
struct Node {
  _header: runtime_native::gc::ObjHeader,
  child: *mut u8,
  value: usize,
}

static SHAPE_TABLE_ONCE: Once = Once::new();

static LEAF_PTR_OFFSETS: [u32; 0] = [];
static NODE_PTR_OFFSETS: [u32; 1] = [core::mem::offset_of!(Node, child) as u32];

static SHAPES: [RtShapeDescriptor; 2] = [
  RtShapeDescriptor {
    size: core::mem::size_of::<Leaf>() as u32,
    align: 16,
    flags: 0,
    ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: core::mem::size_of::<Node>() as u32,
    align: 16,
    flags: 0,
    ptr_offsets: NODE_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: NODE_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn nursery_contains(ptr: *mut u8) -> bool {
  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  // SAFETY: out pointers are valid.
  unsafe { rt_gc_get_young_range(&mut start, &mut end) };
  let addr = ptr as usize;
  addr >= start as usize && addr < end as usize
}

#[test]
fn remembered_set_keeps_young_reachable_from_old() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  // Root the node so it survives collections.
  let mut root_node: *mut u8 = core::ptr::null_mut();
  let h = rt_gc_register_root_slot(&mut root_node as *mut *mut u8);

  // Allocate a node and promote it to old generation.
  root_node = rt_alloc(core::mem::size_of::<Node>(), RtShapeId(2));
  assert!(nursery_contains(root_node));
  unsafe {
    (*(root_node as *mut Node)).child = core::ptr::null_mut();
    (*(root_node as *mut Node)).value = 999;
  }
  rt_gc_collect();
  assert!(
    !nursery_contains(root_node),
    "expected rooted node to be evacuated out of nursery"
  );

  // Allocate a young leaf and store it into the old node.
  let leaf = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
  assert!(nursery_contains(leaf));
  unsafe {
    (*(leaf as *mut Leaf)).value = 42;
    (*(root_node as *mut Node)).child = leaf;
  }

  // Call the exported write barrier after the store so it can read the newly stored value.
  unsafe {
    let slot = core::ptr::addr_of_mut!((*(root_node as *mut Node)).child) as *mut u8;
    rt_write_barrier(root_node, slot);
  }

  assert!(
    remembered_set_contains(root_node),
    "expected write barrier to record old object in remembered set"
  );

  // Without the remembered set scan, the young leaf would be collected (it is only reachable from
  // an old object, and minor GC does not scan all old objects).
  rt_gc_collect();

  assert!(
    !remembered_set_contains(root_node),
    "minor GC should clear the remembered set after evacuating nursery"
  );

  let child = unsafe { (*(root_node as *mut Node)).child };
  assert!(!child.is_null());
  assert!(
    !nursery_contains(child),
    "young leaf must be promoted and the node.child slot updated"
  );
  unsafe {
    assert_eq!((*(child as *mut Leaf)).value, 42);
    assert_eq!((*(root_node as *mut Node)).value, 999);
  }

  rt_gc_unregister_root_slot(h);
  rt_thread_deinit();
}

