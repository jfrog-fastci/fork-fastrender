use std::sync::Arc;

use runtime_native::nursery::{NurserySpace, ThreadNursery, TLAB_SIZE};
use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn single_thread_alignment() {
  let _rt = TestRuntimeGuard::new();
  let nursery = NurserySpace::new(1024 * 1024).unwrap();
  let mut tn = ThreadNursery::new();

  let cases = [(1usize, 1usize), (3, 2), (8, 8), (24, 8), (16, 16), (7, 32)];

  let mut last = 0usize;
  for (size, align) in cases {
    let ptr = tn.alloc(size, align, &nursery).unwrap();
    assert_eq!((ptr as usize) % align, 0);
    assert!(nursery.contains(ptr));
    assert!(unsafe { ptr.add(size) } <= nursery.end());
    assert!((ptr as usize) >= last);
    last = ptr as usize;
  }
}

#[test]
fn refill_path_and_large_alloc() {
  let _rt = TestRuntimeGuard::new();
  let nursery = NurserySpace::new(2 * 1024 * 1024).unwrap();
  let mut tn = ThreadNursery::new();

  // Exhaust multiple TLABs.
  let mut ptrs = Vec::new();
  for _ in 0..(TLAB_SIZE / 1024) * 4 {
    let p = tn.alloc(1024, 8, &nursery).unwrap();
    ptrs.push(p as usize);
  }
  // Allocations should be unique/non-overlapping.
  ptrs.sort_unstable();
  ptrs.dedup();
  assert_eq!(ptrs.len(), (TLAB_SIZE / 1024) * 4);

  assert!(nursery.allocated_bytes() >= TLAB_SIZE * 4);

  // Allocate a large object that bypasses the TLAB.
  let before = nursery.allocated_bytes();
  let big = tn.alloc(TLAB_SIZE + 128, 16, &nursery).unwrap();
  assert_eq!((big as usize) % 16, 0);
  assert!(nursery.contains(big));
  assert!(nursery.allocated_bytes() - before >= TLAB_SIZE + 128);
}

#[test]
fn reset_resets_bump_pointer() {
  let _rt = TestRuntimeGuard::new();
  let nursery = NurserySpace::new(1024 * 1024).unwrap();
  let mut tn = ThreadNursery::new();

  let p1 = tn.alloc(8, 8, &nursery).unwrap();
  assert_eq!(p1 as usize, nursery.start() as usize);
  assert!(nursery.allocated_bytes() > 0);

  // SAFETY: This test is single-threaded and does not use the old TLAB after
  // reset.
  unsafe { nursery.reset() };
  assert_eq!(nursery.allocated_bytes(), 0);

  let mut tn2 = ThreadNursery::new();
  let p2 = tn2.alloc(8, 8, &nursery).unwrap();
  assert_eq!(p2 as usize, nursery.start() as usize);
}

#[test]
fn multithread_allocations_do_not_overlap() {
  let _rt = TestRuntimeGuard::new();
  let nursery = Arc::new(NurserySpace::new(16 * 1024 * 1024).unwrap());

  let threads = 8;
  let iters = 2000;

  let mut handles = Vec::new();
  for _ in 0..threads {
    let nursery = nursery.clone();
    handles.push(std::thread::spawn(move || {
      let mut tn = ThreadNursery::new();
      let mut ranges = Vec::with_capacity(iters);
      for i in 0..iters {
        let (size, align) = if i % 100 == 0 {
          (TLAB_SIZE + 128, 16)
        } else {
          (24, 8)
        };
        let ptr = tn.alloc(size, align, &nursery).unwrap();
        ranges.push((ptr as usize, size));
      }
      ranges
    }));
  }

  let mut all = Vec::new();
  for h in handles {
    all.extend(h.join().unwrap());
  }

  all.sort_by_key(|(start, _)| *start);
  for win in all.windows(2) {
    let (a_start, a_size) = win[0];
    let (b_start, _) = win[1];
    let a_end = a_start + a_size;
    assert!(
      a_end <= b_start,
      "overlap detected: [{a_start:#x}, {a_end:#x}) overlaps next start {b_start:#x}"
    );
  }
}
