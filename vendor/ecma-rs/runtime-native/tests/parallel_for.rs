use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use runtime_native::abi::TaskId;
use runtime_native::{rt_parallel_for, rt_parallel_join, rt_parallel_spawn};
use runtime_native::test_util::TestRuntimeGuard;

type TaskFn = extern "C" fn(*mut u8);

extern "C" fn sum_body(i: usize, data: *mut u8) {
  let sum = unsafe { &*(data as *const AtomicU64) };
  sum.fetch_add(i as u64, Ordering::Relaxed);
}

#[test]
fn correctness_sum() {
  let _rt = TestRuntimeGuard::new();
  let sum = AtomicU64::new(0);
  let n: usize = 1_000_000;

  rt_parallel_for(0, n, sum_body, (&sum as *const AtomicU64).cast_mut().cast());

  let expected = (n as u64) * ((n - 1) as u64) / 2;
  assert_eq!(sum.load(Ordering::Relaxed), expected);
}

extern "C" fn inc_body(_: usize, data: *mut u8) {
  let count = unsafe { &*(data as *const AtomicUsize) };
  count.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn small_range_sequential_fallback() {
  let _rt = TestRuntimeGuard::new();
  let count = AtomicUsize::new(0);
  let start = 123;
  let end = 123 + 16;

  rt_parallel_for(
    start,
    end,
    inc_body,
    (&count as *const AtomicUsize).cast_mut().cast(),
  );

  assert_eq!(count.load(Ordering::Relaxed), end - start);
}

#[repr(C)]
struct NestedTaskData {
  out: *const AtomicUsize,
}

extern "C" fn nested_task(data: *mut u8) {
  let data = unsafe { &*(data as *const NestedTaskData) };
  let out = unsafe { &*data.out };
  out.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn nested_body(i: usize, data: *mut u8) {
  let out = unsafe { &*(data as *const AtomicUsize) };
  out.fetch_add(1, Ordering::Relaxed);

  if i % 1024 == 0 {
    let nested_data = NestedTaskData {
      out: out as *const AtomicUsize,
    };

    let mut tasks: [TaskId; 4] = [TaskId(0); 4];
    for task in &mut tasks {
      *task = rt_parallel_spawn(
        nested_task as TaskFn,
        (&nested_data as *const NestedTaskData).cast_mut().cast(),
      );
    }
    rt_parallel_join(tasks.as_ptr(), tasks.len());
  }
}

#[test]
fn nested_spawn_join_no_deadlock() {
  let _rt = TestRuntimeGuard::new();
  let out = AtomicUsize::new(0);
  let n = 16 * 1024;
  rt_parallel_for(
    0,
    n,
    nested_body,
    (&out as *const AtomicUsize).cast_mut().cast(),
  );

  let nested_count = ((n + 1023) / 1024) * 4;
  assert_eq!(out.load(Ordering::Relaxed), n + nested_count);
}
