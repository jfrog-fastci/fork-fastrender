#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Once;

use runtime_native::abi::RtShapeDescriptor;
use runtime_native::abi::RtShapeId;
use runtime_native::shape_table::rt_register_shape_table;
use runtime_native::test_util::TestRuntimeGuard;

const SHAPE: RtShapeId = RtShapeId(1);
const OBJ_SIZE: usize = runtime_native::gc::OBJ_HEADER_SIZE;

static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: OBJ_SIZE as u32,
  align: 16,
  flags: 0,
  ptr_offsets: std::ptr::null(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table_registered() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[test]
fn stw_gc_does_not_deadlock_with_many_mutators() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table_registered();

  runtime_native::rt_thread_init(0);

  let workers = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4)
    .min(4);

  let start = Arc::new(Barrier::new(workers + 1));

  let mut handles = Vec::with_capacity(workers);
  for _ in 0..workers {
    let start = start.clone();
    handles.push(std::thread::spawn(move || {
      runtime_native::rt_thread_init(1);
      start.wait();

      for _ in 0..10_000 {
        let obj = runtime_native::rt_alloc(OBJ_SIZE, SHAPE);
        std::hint::black_box(obj);
        runtime_native::rt_gc_safepoint();
      }
    }));
  }

  // Ensure mutators are active before we start requesting stop-the-world collections.
  start.wait();

  for _ in 0..50 {
    runtime_native::rt_gc_collect();
  }

  for h in handles {
    h.join().expect("worker panicked");
  }
}
