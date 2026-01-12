use ahash::AHashMap;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use parking_lot::RwLock;
use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use types_ts_interned::TypeStore;

#[derive(Default)]
struct LockedNameMap {
  map: RwLock<AHashMap<String, u64>>,
}

impl LockedNameMap {
  fn with_prefilled(names: &[String]) -> Self {
    let mut map = AHashMap::new();
    for (idx, name) in names.iter().enumerate() {
      map.insert(name.clone(), idx as u64);
    }
    Self { map: RwLock::new(map) }
  }

  /// Baseline: always take an exclusive lock even for existing names.
  fn intern_write_locked(&self, name: &str) -> u64 {
    let mut guard = self.map.write();
    if let Some(id) = guard.get(name) {
      return *id;
    }
    let id = guard.len() as u64;
    guard.insert(name.to_owned(), id);
    id
  }

  /// Read-fast path: take a shared read lock for existing names.
  fn intern_read_then_write(&self, name: &str) -> u64 {
    if let Some(id) = self.map.read().get(name).copied() {
      return id;
    }
    let mut guard = self.map.write();
    if let Some(id) = guard.get(name) {
      return *id;
    }
    let id = guard.len() as u64;
    guard.insert(name.to_owned(), id);
    id
  }
}

fn bench_names(c: &mut Criterion) {
  let mut group = c.benchmark_group("types-ts-interned/names");

  let names: Vec<String> = (0..1024).map(|i| format!("name_{i:04}")).collect();

  group.bench_function("cold_insert/store_intern_name_ref_1024", |b| {
    b.iter_batched(
      TypeStore::new,
      |store| {
        for name in &names {
          black_box(store.intern_name_ref(name));
        }
      },
      BatchSize::LargeInput,
    );
  });

  let store = TypeStore::new();
  for name in &names {
    store.intern_name_ref(name);
  }
  group.bench_function("hot_lookup/store_intern_name_ref_1024", |b| {
    b.iter(|| {
      for name in &names {
        black_box(store.intern_name_ref(name));
      }
    });
  });

  let baseline_write = LockedNameMap::with_prefilled(&names);
  group.bench_function("hot_lookup/baseline_write_lock_1024", |b| {
    b.iter(|| {
      for name in &names {
        black_box(baseline_write.intern_write_locked(name));
      }
    });
  });

  let baseline_read = LockedNameMap::with_prefilled(&names);
  group.bench_function("hot_lookup/baseline_read_then_write_1024", |b| {
    b.iter(|| {
      for name in &names {
        black_box(baseline_read.intern_read_then_write(name));
      }
    });
  });

  // Criterion itself is single-threaded, but name interning is often a
  // contention point under parallel type checking. Run a fixed thread fanout
  // inside the benchmark to capture scalability differences.
  let thread_count = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4)
    .min(8)
    .max(2);
  let iters = 1_000usize;

  // Use a smaller working set here to increase contention on the interner lock
  // rather than spending time hashing many distinct strings.
  let hot_names: Arc<Vec<String>> = Arc::new((0..128).map(|i| format!("hot_{i:04}")).collect());

  let store = TypeStore::new();
  for name in hot_names.iter() {
    store.intern_name_ref(name);
  }
  group.bench_function("hot_lookup_mt/store_intern_name_ref", |b| {
    b.iter(|| {
      thread::scope(|scope| {
        for _ in 0..thread_count {
          let store = Arc::clone(&store);
          let names = Arc::clone(&hot_names);
          scope.spawn(move || {
            for _ in 0..iters {
              for name in names.iter() {
                black_box(store.intern_name_ref(name));
              }
            }
          });
        }
      });
    });
  });

  let baseline_write = Arc::new(LockedNameMap::with_prefilled(&hot_names));
  group.bench_function("hot_lookup_mt/baseline_write_lock", |b| {
    b.iter(|| {
      thread::scope(|scope| {
        for _ in 0..thread_count {
          let interner = Arc::clone(&baseline_write);
          let names = Arc::clone(&hot_names);
          scope.spawn(move || {
            for _ in 0..iters {
              for name in names.iter() {
                black_box(interner.intern_write_locked(name));
              }
            }
          });
        }
      });
    });
  });

  let baseline_read = Arc::new(LockedNameMap::with_prefilled(&hot_names));
  group.bench_function("hot_lookup_mt/baseline_read_then_write", |b| {
    b.iter(|| {
      thread::scope(|scope| {
        for _ in 0..thread_count {
          let interner = Arc::clone(&baseline_read);
          let names = Arc::clone(&hot_names);
          scope.spawn(move || {
            for _ in 0..iters {
              for name in names.iter() {
                black_box(interner.intern_read_then_write(name));
              }
            }
          });
        }
      });
    });
  });

  group.finish();
}

criterion_group!(benches, bench_names);
criterion_main!(benches);

