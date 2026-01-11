use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

extern "C" fn empty_task(_data: *mut u8) {}

#[repr(C)]
struct CpuCtx {
  iters: u32,
}

extern "C" fn cpu_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const CpuCtx) };
  let mut x = 0u64;
  for i in 0..ctx.iters {
    x = x.wrapping_add(i as u64);
  }
  criterion::black_box(x);
}

fn bench_scheduler_throughput(c: &mut Criterion) {
  // Ensure the global scheduler is initialized before we start measuring.
  let warm = runtime_native::rt_parallel_spawn(empty_task, std::ptr::null_mut());
  runtime_native::rt_parallel_join(&warm as *const _, 1);

  let mut group = c.benchmark_group("scheduler_throughput");
  group.measurement_time(Duration::from_secs(1));
  group.warm_up_time(Duration::from_millis(200));

  for &tasks in &[1024usize, 8192] {
    group.throughput(Throughput::Elements(tasks as u64));
    group.bench_with_input(BenchmarkId::new("empty", tasks), &tasks, |b, &tasks| {
      b.iter(|| {
        let mut hs = Vec::with_capacity(tasks);
        for _ in 0..tasks {
          hs.push(runtime_native::rt_parallel_spawn(
            empty_task,
            std::ptr::null_mut(),
          ));
        }
        runtime_native::rt_parallel_join(hs.as_ptr(), hs.len());
      });
    });
  }

  let cpu_ctx = CpuCtx { iters: 256 };
  for &tasks in &[256usize, 1024] {
    group.throughput(Throughput::Elements(tasks as u64));
    group.bench_with_input(
      BenchmarkId::new("cpu_256iters", tasks),
      &tasks,
      |b, &tasks| {
        b.iter(|| {
          let mut hs = Vec::with_capacity(tasks);
          for _ in 0..tasks {
            hs.push(runtime_native::rt_parallel_spawn(
              cpu_task,
              &cpu_ctx as *const _ as *mut u8,
            ));
          }
          runtime_native::rt_parallel_join(hs.as_ptr(), hs.len());
        });
      },
    );
  }

  group.finish();
}

criterion_group!(name = benches; config = Criterion::default(); targets = bench_scheduler_throughput);
criterion_main!(benches);
