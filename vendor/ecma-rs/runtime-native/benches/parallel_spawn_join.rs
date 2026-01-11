use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

#[repr(C)]
struct PayloadCtx {
  ptr: *const u8,
  len: usize,
}

extern "C" fn payload_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const PayloadCtx) };
  let mut sum = 0u64;
  for i in 0..ctx.len {
    sum = sum.wrapping_add(unsafe { *ctx.ptr.add(i) } as u64);
  }
  criterion::black_box(sum);
}

extern "C" fn noop(_data: *mut u8) {}

fn bench_parallel_spawn_join(c: &mut Criterion) {
  // Ensure the global scheduler is initialized before we start measuring.
  let warm = runtime_native::rt_parallel_spawn(noop, std::ptr::null_mut());
  runtime_native::rt_parallel_join(&warm as *const _, 1);

  let mut group = c.benchmark_group("parallel_spawn_join");
  group.measurement_time(Duration::from_secs(1));
  group.warm_up_time(Duration::from_millis(200));

  let task_counts = [1usize, 8, 64, 512];
  let payload_sizes = [0usize, 64, 1024];

  for &tasks in &task_counts {
    for &payload in &payload_sizes {
      let buf = vec![1u8; tasks.saturating_mul(payload.max(1))];
      let ctxs: Vec<PayloadCtx> = (0..tasks)
        .map(|i| PayloadCtx {
          ptr: buf.as_ptr().wrapping_add(i * payload),
          len: payload,
        })
        .collect();

      group.throughput(Throughput::Elements(tasks as u64));
      group.bench_with_input(
        BenchmarkId::new(format!("tasks={tasks}"), format!("payload={payload}")),
        &payload,
        |b, _payload| {
          b.iter(|| {
            let mut hs = Vec::with_capacity(tasks);
            for ctx in &ctxs {
              hs.push(runtime_native::rt_parallel_spawn(
                payload_task,
                ctx as *const _ as *mut u8,
              ));
            }
            runtime_native::rt_parallel_join(hs.as_ptr(), hs.len());
          });
        },
      );
    }
  }
  group.finish();
}

criterion_group!(name = benches; config = Criterion::default(); targets = bench_parallel_spawn_join);
criterion_main!(benches);
