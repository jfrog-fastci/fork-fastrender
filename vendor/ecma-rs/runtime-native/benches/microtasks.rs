use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use runtime_native::async_rt;

extern "C" fn noop(_data: *mut u8) {}

fn bench_microtasks(c: &mut Criterion) {
  let rt = async_rt::global();

  let mut group = c.benchmark_group("microtasks");
  group.measurement_time(Duration::from_secs(1));
  group.warm_up_time(Duration::from_millis(200));

  for &n in &[1024usize, 8192, 65536] {
    group.throughput(Throughput::Elements(n as u64));
    group.bench_with_input(BenchmarkId::new("enqueue_drain", n), &n, |b, &n| {
      b.iter(|| {
        for _ in 0..n {
          async_rt::enqueue_microtask(noop, std::ptr::null_mut());
        }
        let pending = rt.poll();
        criterion::black_box(pending);
      });
    });
  }

  group.finish();
}

criterion_group!(name = benches; config = Criterion::default(); targets = bench_microtasks);
criterion_main!(benches);
