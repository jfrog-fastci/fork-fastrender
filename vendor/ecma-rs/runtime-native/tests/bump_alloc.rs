use std::process::Command;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::rt_alloc;
use runtime_native::rt_alloc_array;
use runtime_native::rt_alloc_pinned;
use runtime_native::shape_table;

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 5] = [
  RtShapeDescriptor {
    size: 16,
    align: 16,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: 24,
    align: 16,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: 40,
    align: 16,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  RtShapeDescriptor {
    size: 256,
    align: 16,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  // Stronger alignment than the runtime's baseline.
  RtShapeDescriptor {
    size: 32,
    align: 32,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn round_up_16(n: usize) -> usize {
  (n + 15) & !15
}

#[test]
fn alloc_alignment() {
  ensure_shape_table();
  for _ in 0..128 {
    let ptr = rt_alloc(16, RtShapeId(1));
    assert_eq!((ptr as usize) & 15, 0);
  }
}

#[test]
fn alloc_distinct() {
  ensure_shape_table();
  let a_size = 24;
  let b_size = 40;
  let a = rt_alloc(a_size, RtShapeId(2)) as usize;
  let b = rt_alloc(b_size, RtShapeId(3)) as usize;

  let a_end = a + round_up_16(a_size);
  let b_end = b + round_up_16(b_size);

  assert!(a_end <= b || b_end <= a, "allocations overlapped");
}

#[test]
fn alloc_honors_shape_alignment() {
  ensure_shape_table();
  for _ in 0..128 {
    let ptr = rt_alloc(32, RtShapeId(5));
    assert_eq!((ptr as usize) & 31, 0);
  }
}

#[test]
fn alloc_pinned_honors_shape_alignment() {
  ensure_shape_table();
  for _ in 0..128 {
    let ptr = rt_alloc_pinned(32, RtShapeId(5));
    assert_eq!((ptr as usize) & 31, 0);
  }
}

#[test]
fn alloc_array_overflow_child() {
  if std::env::var_os("RT_ALLOC_OVERFLOW_CHILD").is_none() {
    return;
  }

  let _ = rt_alloc_array(usize::MAX, 2);
  panic!("rt_alloc_array should have aborted or panicked on overflow");
}

#[test]
fn alloc_array_overflow_aborts_or_panics() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_ALLOC_OVERFLOW_CHILD", "1")
    .arg("--exact")
    .arg("alloc_array_overflow_child")
    .status()
    .expect("spawn child");

  assert!(!status.success(), "expected child to abort/panic");
}

#[test]
fn register_shape_table_invalid_align_child() {
  if std::env::var_os("RT_SHAPE_ALIGN_CHILD").is_none() {
    return;
  }

  unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: 16,
      align: 24, // invalid: not a power of two
      flags: 0,
      ptr_offsets: core::ptr::null(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  }

  panic!("rt_register_shape_table should have aborted or panicked on invalid alignment");
}

#[test]
fn register_shape_table_invalid_align_aborts_or_panics() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_SHAPE_ALIGN_CHILD", "1")
    .arg("--exact")
    .arg("register_shape_table_invalid_align_child")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .expect("spawn child");

  assert!(!status.success(), "expected child to abort/panic");
}

#[test]
fn thread_local_fast_path() {
  ensure_shape_table();
  const THREADS: usize = 8;
  const ITERS: usize = 10_000;
  const SIZE: usize = 256;

  let mut handles = Vec::with_capacity(THREADS);
  for _ in 0..THREADS {
    handles.push(std::thread::spawn(|| {
      ensure_shape_table();
      let mut ranges = Vec::with_capacity(ITERS);
      for _ in 0..ITERS {
        let ptr = rt_alloc(SIZE, RtShapeId(4)) as usize;
        let end = ptr.checked_add(round_up_16(SIZE)).expect("ptr overflow");
        ranges.push((ptr, end));
      }
      ranges
    }));
  }

  let mut all = Vec::with_capacity(THREADS * ITERS);
  for h in handles {
    all.extend(h.join().expect("thread panicked"));
  }

  all.sort_unstable_by_key(|(start, _)| *start);
  for w in all.windows(2) {
    let (_, a_end) = w[0];
    let (b_start, _) = w[1];
    assert!(a_end <= b_start, "overlapping allocations across threads");
  }
}
