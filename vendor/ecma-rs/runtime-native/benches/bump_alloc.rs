// Benchmarks for the `rt_alloc` fast paths.
//
// Note: `rt_alloc` allocates into the GC heap (nursery/Immix/LOS). The benchmark does not keep
// the returned pointers live across allocations, so collections are free to reclaim everything.
// This makes it suitable for measuring allocation throughput without needing explicit rooting.

#[cfg(target_os = "linux")]
use std::sync::Once;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use criterion::black_box;
#[cfg(target_os = "linux")]
use criterion::criterion_group;
#[cfg(target_os = "linux")]
use criterion::criterion_main;
#[cfg(target_os = "linux")]
use criterion::Criterion;
#[cfg(target_os = "linux")]
use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
#[cfg(target_os = "linux")]
use runtime_native::shape_table;

#[cfg(target_os = "linux")]
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
#[cfg(target_os = "linux")]
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: 16,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

#[cfg(target_os = "linux")]
fn init_for_bench() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| {
    unsafe {
      shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
    }

    // Ensure one-time runtime initialization doesn't get charged to the benchmark loop.
    black_box(runtime_native::rt_alloc(16, RtShapeId(1)));
  });
}

#[cfg(target_os = "linux")]
fn rt_alloc_single_thread(c: &mut Criterion) {
  init_for_bench();

  c.bench_function("rt_alloc_16B_single_thread", |b| {
    b.iter(|| {
      let ptr = runtime_native::rt_alloc(16, RtShapeId(1));
      black_box(ptr);
    });
  });
}

#[cfg(target_os = "linux")]
fn rt_alloc_multi_thread(c: &mut Criterion) {
  init_for_bench();

  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(8)
    .min(8);

  c.bench_function("rt_alloc_16B_multi_thread", |b| {
    b.iter_custom(|iters| {
      let total = iters as usize;
      let per = total / threads;
      let rem = total % threads;

      let start = Instant::now();
      let mut handles = Vec::with_capacity(threads);
      for t in 0..threads {
        let n = per + usize::from(t < rem);
        handles.push(std::thread::spawn(move || {
          for _ in 0..n {
            black_box(runtime_native::rt_alloc(16, RtShapeId(1)));
          }
        }));
      }
      for h in handles {
        h.join().unwrap();
      }
      start.elapsed()
    });
  });
}

#[cfg(target_os = "linux")]
criterion_group! {
  name = bump_alloc;
  // Keep the run short and avoid allocating past the (still finite) arena.
  config = Criterion::default()
    .sample_size(10)
    .warm_up_time(Duration::from_millis(100))
    .measurement_time(Duration::from_millis(200));
  targets = rt_alloc_single_thread, rt_alloc_multi_thread
}

#[cfg(target_os = "linux")]
criterion_main!(bump_alloc);

#[cfg(not(target_os = "linux"))]
fn main() {}
