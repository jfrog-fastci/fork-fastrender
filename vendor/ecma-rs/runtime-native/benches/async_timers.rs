use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use runtime_native::async_rt::{AsyncRuntime, Task, TaskFn, Timers};

extern "C" fn noop(_data: *mut u8) {}

extern "C" fn set_flag(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::Release);
}

fn bench_async_timers(c: &mut Criterion) {
  let mut group = c.benchmark_group("timer_heap");
  group.measurement_time(Duration::from_secs(1));
  group.warm_up_time(Duration::from_millis(200));

  for &n in &[1024usize, 8192] {
    group.throughput(Throughput::Elements(n as u64));
    group.bench_with_input(BenchmarkId::new("insert", n), &n, |b, &n| {
      b.iter(|| {
        let timers = Timers::new();
        let now = std::time::Instant::now();
        for i in 0..n {
          timers.schedule(
            now + Duration::from_nanos(i as u64),
            Task::new(noop as TaskFn, std::ptr::null_mut()),
          );
        }
        criterion::black_box(timers.next_deadline());
      });
    });

    group.throughput(Throughput::Elements(n as u64));
    group.bench_with_input(BenchmarkId::new("dispatch_ready", n), &n, |b, &n| {
      b.iter(|| {
        let timers = Timers::new();
        let now = std::time::Instant::now();
        for _ in 0..n {
          timers.schedule(now, Task::new(noop as TaskFn, std::ptr::null_mut()));
        }
        let ready = timers.drain_due(now);
        criterion::black_box(ready.len());
      });
    });
  }
  group.finish();

  let mut group = c.benchmark_group("timer_accuracy");
  group.sample_size(10);
  group.measurement_time(Duration::from_secs(2));
  group.warm_up_time(Duration::from_millis(200));

  let rt = AsyncRuntime::new().unwrap();
  for &delay_ms in &[1u64, 5, 10] {
    group.bench_with_input(
      BenchmarkId::new("delay_ms", delay_ms),
      &delay_ms,
      |b, &delay_ms| {
        b.iter(|| {
          let flag = AtomicBool::new(false);
          rt.schedule_timer_in(
            Duration::from_millis(delay_ms),
            Task::new(set_flag as TaskFn, &flag as *const _ as *mut u8),
          );
          while !flag.load(Ordering::Acquire) {
            rt.poll();
          }
        });
      },
    );
  }
  group.finish();
}

criterion_group!(name = benches; config = Criterion::default(); targets = bench_async_timers);
criterion_main!(benches);
