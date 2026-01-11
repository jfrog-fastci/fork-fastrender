use runtime_native::gc::ObjHeader;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::Arc;
use std::sync::Barrier;

#[repr(C, align(16))]
struct DummyObject {
  header: ObjHeader,
  field: *mut u8,
}

#[test]
fn write_barrier_is_idempotent_under_concurrency() {
  let _rt = TestRuntimeGuard::new();

  let mut young_byte = Box::new(0u8);
  let young_ptr = (&mut *young_byte) as *mut u8;
  unsafe {
    runtime_native::rt_gc_set_young_range(young_ptr, young_ptr.add(1));
  }

  let mut old = Box::new(DummyObject {
    // The write barrier only touches atomic metadata + doesn't require a valid type descriptor.
    header: unsafe { std::mem::zeroed() },
    field: young_ptr,
  });

  let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
  let slot_ptr = (&mut old.field) as *mut *mut u8 as *mut u8;
  let obj_addr = obj_ptr as usize;
  let slot_addr = slot_ptr as usize;

  const THREADS: usize = 8;
  const ITERS: usize = 10_000;

  let start = Arc::new(Barrier::new(THREADS));
  let mut handles = Vec::with_capacity(THREADS);
  for _ in 0..THREADS {
    let start = start.clone();
    handles.push(std::thread::spawn(move || {
      let obj_ptr = obj_addr as *mut u8;
      let slot_ptr = slot_addr as *mut u8;
      start.wait();
      for _ in 0..ITERS {
        unsafe {
          runtime_native::rt_write_barrier(obj_ptr, slot_ptr);
        }
      }
    }));
  }

  for h in handles {
    h.join().unwrap();
  }

  assert!(runtime_native::remembered_set_contains(obj_ptr));
  assert_eq!(runtime_native::remembered_set_len_for_tests(), 1);

  // Ensure `TestRuntimeGuard` teardown cannot observe stale pointers from the global remembered
  // set after `old` is freed.
  runtime_native::clear_write_barrier_state_for_tests();
  runtime_native::rt_gc_set_young_range(std::ptr::null_mut(), std::ptr::null_mut());
}
