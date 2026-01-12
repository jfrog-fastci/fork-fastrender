use std::process::Command;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::rt_alloc;
use runtime_native::rt_gc_collect_minor;
use runtime_native::rt_gc_get_young_range;
use runtime_native::rt_root_pop;
use runtime_native::rt_root_push;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: 256,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[test]
fn gc_collect_minor_evacuates_nursery_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_COLLECT_MINOR_CHILD").is_none() {
    return;
  }

  ensure_shape_table();

  // Ensure the process-global heap (and exported young range) are initialized
  // before reading `rt_gc_get_young_range`.
  let mut root = rt_alloc(256, RtShapeId(1));

  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null(), "expected young range to be initialized");
  assert!(!young_end.is_null(), "expected young range to be initialized");
  assert!(young_start < young_end, "invalid young range");

  let root_before = Box::new(root as usize);
  assert!(
    (young_start as usize..young_end as usize).contains(&*root_before),
    "expected rooted object to be allocated in the nursery"
  );
  unsafe {
    rt_root_push(&mut root as *mut *mut u8);
  }

  rt_gc_collect_minor();

  // Note: the GC may conservatively scan and mutate stack slots that *look*
  // like GC pointers (this is expected in debug builds when there are no
  // stackmaps for Rust frames). Re-read the young range after the collection
  // so our assertions aren't comparing against stack-scribbled locals.
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }

  let root_after = root as usize;
  assert!(
    !(young_start as usize..young_end as usize).contains(&root_after),
    "expected rooted object to be evacuated to old-gen by rt_gc_collect_minor"
  );
  assert_ne!(
    root_after, *root_before,
    "expected rt_gc_collect_minor to update rooted pointer after evacuation"
  );

  unsafe {
    rt_root_pop(&mut root as *mut *mut u8);
  }
}

#[test]
fn gc_collect_minor_evacuates_nursery() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_GC_COLLECT_MINOR_CHILD", "1")
    .arg("--exact")
    .arg("gc_collect_minor_evacuates_nursery_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
}
