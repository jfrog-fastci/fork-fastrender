use std::process::Command;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::{rt_alloc, rt_alloc_pinned};

static INIT_SHAPES: Once = Once::new();

// A minimal leaf shape: no pointer fields, size includes the ObjHeader.
static LEAF_PTR_OFFSETS: [u32; 0] = [];
const LEAF_SIZE: u32 = runtime_native::gc::OBJ_HEADER_SIZE as u32;
const LEAF_ALIGN: u16 = std::mem::align_of::<runtime_native::gc::ObjHeader>() as u16;
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: LEAF_SIZE,
  align: LEAF_ALIGN,
  flags: 0,
  ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

fn ensure_shape_table() {
  INIT_SHAPES.call_once(|| unsafe {
    runtime_native::shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[test]
fn alloc_shaped_succeeds_with_exact_size() {
  ensure_shape_table();

  let ptr = rt_alloc(SHAPES[0].size as usize, RtShapeId(1));
  assert!(!ptr.is_null());
}

#[test]
fn alloc_pinned_shaped_succeeds_with_exact_size() {
  ensure_shape_table();

  let ptr = rt_alloc_pinned(SHAPES[0].size as usize, RtShapeId(1));
  assert!(!ptr.is_null());
}

#[test]
fn alloc_shaped_size_mismatch_child() {
  if std::env::var_os("RT_ALLOC_SIZE_MISMATCH_CHILD").is_none() {
    return;
  }

  ensure_shape_table();

  let _ = rt_alloc((SHAPES[0].size as usize) + 8, RtShapeId(1));
}

#[test]
fn alloc_shaped_size_mismatch_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_ALLOC_SIZE_MISMATCH_CHILD", "1")
    .arg("--exact")
    .arg("alloc_shaped_size_mismatch_child")
    .status()
    .expect("spawn child");

  assert!(!status.success(), "expected child to abort");

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(6), "expected SIGABRT");
  }
}

#[test]
fn alloc_pinned_shaped_size_mismatch_child() {
  if std::env::var_os("RT_ALLOC_PINNED_SIZE_MISMATCH_CHILD").is_none() {
    return;
  }

  ensure_shape_table();

  let _ = rt_alloc_pinned((SHAPES[0].size as usize) + 8, RtShapeId(1));
}

#[test]
fn alloc_pinned_shaped_size_mismatch_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_ALLOC_PINNED_SIZE_MISMATCH_CHILD", "1")
    .arg("--exact")
    .arg("alloc_pinned_shaped_size_mismatch_child")
    .status()
    .expect("spawn child");

  assert!(!status.success(), "expected child to abort");

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(6), "expected SIGABRT");
  }
}
