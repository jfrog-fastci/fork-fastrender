use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::gc::OBJ_HEADER_SIZE;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  register_global_root_slot, rt_alloc, rt_gc_collect, rt_gc_safepoint, rt_thread_deinit, rt_thread_init,
  unregister_global_root_slot,
};

const SHAPE_MARKER: RtShapeId = RtShapeId(1);
const MARKER_PAYLOAD_BYTES: usize = 16;
const MARKER_OBJ_SIZE: usize = OBJ_HEADER_SIZE + MARKER_PAYLOAD_BYTES;

static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: MARKER_OBJ_SIZE as u32,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[inline(always)]
unsafe fn marker_slot(obj: *mut u8) -> *mut u64 {
  obj.add(OBJ_HEADER_SIZE).cast::<u64>()
}

fn wait_until(timeout: Duration, f: impl Fn() -> bool) {
  let start = Instant::now();
  while start.elapsed() < timeout {
    if f() {
      return;
    }
    thread::yield_now();
  }
  panic!("timeout");
}

#[test]
fn alloc_fast_paths_are_invalidated_after_gc() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  const THREADS: usize = 4;
  const ROOTS: usize = 16;
  const NOISE_ALLOCS_PER_TICK: usize = 64;
  const GC_ITERS: usize = 50;
  const CHECK_EVERY_TICKS: usize = 25;

  let stop = Arc::new(AtomicBool::new(false));
  let progress = Arc::new((0..THREADS).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());

  let mut handles = Vec::with_capacity(THREADS);
  for thread_idx in 0..THREADS {
    let stop = Arc::clone(&stop);
    let progress = Arc::clone(&progress);
    handles.push(thread::spawn(move || {
      ensure_shape_table();
      rt_thread_init(1);

      let mut slots = [0usize; ROOTS];
      let mut expected = [0u64; ROOTS];

      // Root the slots up front so GC can update them in-place if objects move.
      for i in 0..ROOTS {
        register_global_root_slot(&mut slots[i] as *mut usize);
      }

      // Seed roots.
      let mut gen: u64 = 1;
      for slot_idx in 0..ROOTS {
        let obj = rt_alloc(MARKER_OBJ_SIZE, SHAPE_MARKER);
        let marker = ((thread_idx as u64) << 32) | ((slot_idx as u64) << 16) | (gen & 0xFFFF);
        unsafe {
          marker_slot(obj).write(marker);
        }
        slots[slot_idx] = obj as usize;
        expected[slot_idx] = marker;
        gen = gen.wrapping_add(1);
      }

      let mut tick = 0usize;
      while !stop.load(Ordering::Relaxed) {
        // Allocate a batch of young objects to keep the nursery hot.
        for _ in 0..NOISE_ALLOCS_PER_TICK {
          let obj = rt_alloc(MARKER_OBJ_SIZE, SHAPE_MARKER);
          // Touch a word in the payload so overlap/corruption is more likely to manifest.
          unsafe {
            marker_slot(obj).write(gen);
          }
          gen = gen.wrapping_add(1);
        }

        // Overwrite one rooted slot with a fresh object so we always have recently-allocated
        // objects that should survive at least one GC and be validated.
        {
          let slot_idx = tick % ROOTS;
          let obj = rt_alloc(MARKER_OBJ_SIZE, SHAPE_MARKER);
          let marker = ((thread_idx as u64) << 32) | ((slot_idx as u64) << 16) | (gen & 0xFFFF);
          unsafe {
            marker_slot(obj).write(marker);
          }
          slots[slot_idx] = obj as usize;
          expected[slot_idx] = marker;
          gen = gen.wrapping_add(1);
        }

        // Cooperatively enter safepoints so `rt_gc_collect` can stop the world.
        rt_gc_safepoint();

        if tick % CHECK_EVERY_TICKS == 0 {
          for i in 0..ROOTS {
            let obj = slots[i] as *mut u8;
            assert!(!obj.is_null());
            let got = unsafe { marker_slot(obj).read() };
            assert_eq!(got, expected[i], "marker corruption in thread {thread_idx} slot {i}");
          }
        }

        progress[thread_idx].fetch_add(1, Ordering::Relaxed);
        tick += 1;
      }

      for i in 0..ROOTS {
        unregister_global_root_slot(&mut slots[i] as *mut usize);
      }

      rt_thread_deinit();
    }));
  }

  // Wait until workers are running so `rt_gc_collect` doesn't immediately time out.
  wait_until(Duration::from_secs(2), || progress.iter().all(|c| c.load(Ordering::Relaxed) > 10));

  // Drive repeated stop-the-world GCs while workers allocate.
  rt_thread_init(3);
  for _ in 0..GC_ITERS {
    rt_gc_collect();
    thread::yield_now();
  }
  rt_thread_deinit();

  // Ensure workers keep making progress after the GC storm.
  let after = progress
    .iter()
    .map(|c| c.load(Ordering::Relaxed))
    .collect::<Vec<_>>();
  wait_until(Duration::from_secs(2), || {
    progress
      .iter()
      .zip(after.iter())
      .all(|(c, before)| c.load(Ordering::Relaxed) > *before)
  });

  stop.store(true, Ordering::Relaxed);
  for h in handles {
    h.join().expect("worker thread panicked");
  }
}
