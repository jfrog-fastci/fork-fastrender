use std::mem;
use std::ptr;
use std::sync::Once;

use runtime_native::abi::RtShapeDescriptor;
use runtime_native::abi::RtShapeId;
use runtime_native::gc::AllocError;
use runtime_native::gc::AllocKind;
use runtime_native::gc::AllocRequest;
use runtime_native::gc::HeapConfig;
use runtime_native::gc::HeapLimits;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootSet;
use runtime_native::GcHeap;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];

const SHAPE_NODE: RtShapeId = RtShapeId(1);
const SHAPE_TMP: RtShapeId = RtShapeId(2);
const SHAPE_BIG: RtShapeId = RtShapeId(3);

const TMP_SIZE: usize = mem::size_of::<ObjHeader>() + 16;
const BIG_SIZE: usize = 256;

static SHAPES: [RtShapeDescriptor; 3] = [
  RtShapeDescriptor {
    size: mem::size_of::<Node>() as u32,
    align: mem::align_of::<Node>() as u16,
    flags: 0,
    ptr_offsets: NODE_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: NODE_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: TMP_SIZE as u32,
    align: mem::align_of::<ObjHeader>() as u16,
    flags: 0,
    ptr_offsets: ptr::null(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: BIG_SIZE as u32,
    align: mem::align_of::<ObjHeader>() as u16,
    flags: 0,
    ptr_offsets: ptr::null(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
];

fn ensure_shape_table() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    runtime_native::shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[derive(Default)]
struct NullRemembered;

impl RememberedSet for NullRemembered {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[derive(Default)]
struct VecRemembered {
  objs: Vec<*mut u8>,
  cleared: bool,
}

impl VecRemembered {
  fn remember(&mut self, obj: *mut u8) {
    if !self.objs.contains(&obj) {
      self.objs.push(obj);
    }
  }
}

impl RememberedSet for VecRemembered {
  fn for_each_remembered_obj(&mut self, f: &mut dyn FnMut(*mut u8)) {
    for &obj in &self.objs {
      f(obj);
    }
  }

  fn clear(&mut self) {
    self.objs.clear();
    self.cleared = true;
  }

  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[derive(Default)]
struct VecRoots(Vec<*mut u8>);

impl RootSet for VecRoots {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for slot in &mut self.0 {
      f(slot as *mut *mut u8);
    }
  }
}

#[test]
fn nursery_fills_triggers_minor_and_preserves_graph() {
  ensure_shape_table();
  let config = HeapConfig {
    nursery_size_bytes: 512,
    los_threshold_bytes: 128,
    minor_gc_nursery_used_percent: 50,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = HeapLimits {
    max_heap_bytes: 16 * 1024 * 1024,
  };
  let mut heap = GcHeap::with_config(config, limits);

  let mut roots = VecRoots::default();
  let mut remembered = VecRemembered::default();

  // Build a small object graph in the nursery.
  let root = heap
    .alloc_object(
      AllocRequest {
        size: mem::size_of::<Node>(),
        align: mem::align_of::<Node>(),
        shape_id: SHAPE_NODE,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    )
    .unwrap();
  roots.0.push(root);

  let child = heap
    .alloc_object(
      AllocRequest {
        size: mem::size_of::<Node>(),
        align: mem::align_of::<Node>(),
        shape_id: SHAPE_NODE,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    )
    .unwrap();

  unsafe {
    (*(root as *mut Node)).next = child;
    (*(child as *mut Node)).next = ptr::null_mut();
  }

  // Fill the nursery with unreachable objects; this should trigger at least one minor GC and
  // evacuate the rooted graph into Immix.
  for _ in 0..200 {
    let _ = heap.alloc_object(
      AllocRequest {
        size: TMP_SIZE,
        align: mem::align_of::<ObjHeader>(),
        shape_id: SHAPE_TMP,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    );
  }
  assert!(heap.stats().minor_collections > 0);

  let root_after = roots.0[0];
  assert!(!heap.is_in_nursery(root_after));
  assert!(heap.is_in_immix(root_after));

  let child_after = unsafe { (*(root_after as *mut Node)).next };
  assert!(!heap.is_in_nursery(child_after));
  assert!(heap.is_in_immix(child_after));

  // Remembered set behaviour: create an old->young edge from a non-root old object.
  let young = heap
    .alloc_object(
      AllocRequest {
        size: mem::size_of::<Node>(),
        align: mem::align_of::<Node>(),
        shape_id: SHAPE_NODE,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    )
    .unwrap();

  unsafe {
    (*(child_after as *mut Node)).next = young;
  }
  remembered.remember(child_after);

  // Force another minor GC.
  for _ in 0..200 {
    let _ = heap.alloc_object(
      AllocRequest {
        size: TMP_SIZE,
        align: mem::align_of::<ObjHeader>(),
        shape_id: SHAPE_TMP,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    );
  }

  let young_after = unsafe { (*(child_after as *mut Node)).next };
  assert!(!heap.is_in_nursery(young_after));
  assert!(heap.is_in_immix(young_after));
  assert!(remembered.cleared, "minor GC should clear the remembered set");
}

#[test]
fn tiny_heap_is_deterministic_oom() {
  ensure_shape_table();
  let config = HeapConfig {
    nursery_size_bytes: 256,
    los_threshold_bytes: 128,
    minor_gc_nursery_used_percent: 100,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };

  // Allow the nursery plus a small number of Immix blocks.
  let limits = HeapLimits {
    max_heap_bytes: 256 * 1024,
  };

  fn run_once(config: HeapConfig, limits: HeapLimits) -> usize {
    let mut heap = GcHeap::with_config(config, limits);
    let mut roots = VecRoots::default();
    let mut remembered = NullRemembered::default();

    let mut ok = 0usize;
    loop {
      match heap.alloc_object(
        AllocRequest {
          size: TMP_SIZE,
          align: mem::align_of::<ObjHeader>(),
          shape_id: SHAPE_TMP,
          kind: Some(AllocKind::OldOnly),
        },
        &mut roots,
        &mut remembered,
      ) {
        Ok(obj) => {
          roots.0.push(obj);
          ok += 1;
        }
        Err(AllocError::OutOfMemory) => break,
      }
    }
    ok
  }

  let a = run_once(config, limits);
  let b = run_once(config, limits);
  assert_eq!(a, b);
  assert!(a > 0);
}

#[test]
fn los_allocations_do_not_consume_nursery() {
  ensure_shape_table();
  let config = HeapConfig {
    nursery_size_bytes: 1024,
    los_threshold_bytes: 128,
    minor_gc_nursery_used_percent: 100,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = HeapLimits {
    max_heap_bytes: 16 * 1024 * 1024,
  };

  let mut heap = GcHeap::with_config(config, limits);
  let mut roots = VecRoots::default();
  let mut remembered = NullRemembered::default();

  let before = heap.nursery_allocated_bytes();

  let big = heap
    .alloc_object(
      AllocRequest {
        size: BIG_SIZE,
        align: mem::align_of::<ObjHeader>(),
        shape_id: SHAPE_BIG,
        kind: None,
      },
      &mut roots,
      &mut remembered,
    )
    .unwrap();
  roots.0.push(big);

  assert_eq!(heap.nursery_allocated_bytes(), before);
  assert!(heap.is_in_los(big));
}
