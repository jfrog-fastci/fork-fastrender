use std::process::Command;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::rt_alloc;
use runtime_native::rt_alloc_array;
use runtime_native::rt_alloc_pinned;
use runtime_native::rt_gc_get_young_range;
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
fn alloc_array_gc_child() {
  if std::env::var_os("RT_ALLOC_ARRAY_GC_CHILD").is_none() {
    return;
  }

  let small = rt_alloc_array(1, 1);
  assert!(!small.is_null());
  assert_eq!((small as usize) & 15, 0);

  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut start, &mut end);
  }
  assert!(!start.is_null(), "expected rt_alloc_array to initialize young range start");
  assert!(!end.is_null(), "expected rt_alloc_array to initialize young range end");
  assert!(start < end, "invalid young range");

  let small_addr = small as usize;
  assert!(
    small_addr >= start as usize && small_addr < end as usize,
    "expected small array to be allocated in the nursery (young range)"
  );

  // Large arrays should go to the LOS and therefore live outside the nursery range.
  let big = rt_alloc_array(IMMIX_MAX_OBJECT_SIZE, 1);
  let big_addr = big as usize;
  assert!(
    big_addr < start as usize || big_addr >= end as usize,
    "expected large array to be allocated outside the nursery (LOS)"
  );

  // Pointer-element arrays must record their element flags in the header.
  let ptr_arr = rt_alloc_array(
    2,
    runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG | core::mem::size_of::<*mut u8>(),
  );
  unsafe {
    let hdr = &*(ptr_arr as *const runtime_native::array::RtArrayHeader);
    assert_eq!(hdr.len, 2);
    assert_eq!(hdr.elem_size as usize, core::mem::size_of::<*mut u8>());
    assert_eq!(hdr.elem_flags, runtime_native::array::RT_ARRAY_FLAG_PTR_ELEMS);
  }
}

#[test]
fn alloc_array_is_gc_backed() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_ALLOC_ARRAY_GC_CHILD", "1")
    .arg("--exact")
    .arg("alloc_array_gc_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
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
