use std::process::Command;

use runtime_native::rt_alloc;
use runtime_native::rt_alloc_array;

fn round_up_16(n: usize) -> usize {
  (n + 15) & !15
}

#[test]
fn alloc_alignment() {
  for _ in 0..128 {
    let ptr = rt_alloc(1, 0);
    assert_eq!((ptr as usize) & 15, 0);
  }
}

#[test]
fn alloc_distinct() {
  let a_size = 24;
  let b_size = 40;
  let a = rt_alloc(a_size, 0) as usize;
  let b = rt_alloc(b_size, 0) as usize;

  let a_end = a + round_up_16(a_size);
  let b_end = b + round_up_16(b_size);

  assert!(a_end <= b || b_end <= a, "allocations overlapped");
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
fn thread_local_fast_path() {
  const THREADS: usize = 8;
  const ITERS: usize = 10_000;
  const SIZE: usize = 256;

  let mut handles = Vec::with_capacity(THREADS);
  for _ in 0..THREADS {
    handles.push(std::thread::spawn(|| {
      let mut ranges = Vec::with_capacity(ITERS);
      for _ in 0..ITERS {
        let ptr = rt_alloc(SIZE, 0) as usize;
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

