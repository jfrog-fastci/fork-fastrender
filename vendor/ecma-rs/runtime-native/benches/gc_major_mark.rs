#[cfg(target_os = "linux")]
use std::mem;
#[cfg(target_os = "linux")]
use std::ptr;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use criterion::black_box;
#[cfg(target_os = "linux")]
use criterion::criterion_group;
#[cfg(target_os = "linux")]
use criterion::criterion_main;
#[cfg(target_os = "linux")]
use criterion::Criterion;
#[cfg(target_os = "linux")]
use runtime_native::gc::{HeapConfig, HeapLimits, ObjHeader, RootStack, SimpleRememberedSet, TypeDescriptor};
#[cfg(target_os = "linux")]
use runtime_native::GcHeap;

#[cfg(target_os = "linux")]
const EXTRA_PTRS: usize = 4;
#[cfg(target_os = "linux")]
#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
  extra: [*mut u8; EXTRA_PTRS],
}

#[cfg(target_os = "linux")]
static NODE_PTR_OFFSETS: [u32; 1 + EXTRA_PTRS] = {
  const PTR_SIZE: usize = mem::size_of::<*mut u8>();
  let base = mem::offset_of!(Node, extra) as u32;
  [
    mem::offset_of!(Node, next) as u32,
    base + (0 * PTR_SIZE) as u32,
    base + (1 * PTR_SIZE) as u32,
    base + (2 * PTR_SIZE) as u32,
    base + (3 * PTR_SIZE) as u32,
  ]
};
#[cfg(target_os = "linux")]
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[cfg(target_os = "linux")]
struct BenchHeap {
  heap: GcHeap,
  roots: RootStack,
  remembered: SimpleRememberedSet,
  root_slots: Vec<Box<*mut u8>>,
}

#[cfg(target_os = "linux")]
fn build_heap(mark_threads: usize, nodes: usize, chains: usize) -> BenchHeap {
  let config = HeapConfig {
    major_gc_mark_threads: mark_threads,
    ..HeapConfig::default()
  };
  let limits = HeapLimits::default();
  let mut heap = GcHeap::with_config(config, limits);
  let mut roots = RootStack::new();
  let remembered = SimpleRememberedSet::new();

  let chains = chains.max(1).min(nodes.max(1));
  let base = nodes / chains;
  let mut extra = nodes % chains;

  // Build a forest of linked lists (many independent roots) so marking has enough parallelism to
  // keep multiple worker threads busy.
  let mut root_slots = Vec::with_capacity(chains);
  for _ in 0..chains {
    let mut chain_len = base;
    if extra > 0 {
      chain_len += 1;
      extra -= 1;
    }
    if chain_len == 0 {
      continue;
    }

    let mut slot = Box::new(ptr::null_mut());
    *slot = heap.alloc_old(&NODE_DESC);
    roots.push(&mut *slot as *mut *mut u8);

    unsafe {
      let n = &mut *(*slot as *mut Node);
      n.next = ptr::null_mut();
      // Keep some extra pointer slots non-null to increase per-object scan work.
      n.extra = [*slot; EXTRA_PTRS];
    }

    let mut prev = *slot;
    for _ in 1..chain_len {
      let obj = heap.alloc_old(&NODE_DESC);
      unsafe {
        (*(prev as *mut Node)).next = obj;
        (*(obj as *mut Node)).next = ptr::null_mut();
        (*(obj as *mut Node)).extra = [*slot; EXTRA_PTRS];
      }
      prev = obj;
    }
    root_slots.push(slot);
  }

  BenchHeap {
    heap,
    roots,
    remembered,
    root_slots,
  }
}

#[cfg(target_os = "linux")]
fn bench_major_gc_mark(c: &mut Criterion) {
  // Keep the heap large enough that marking dominates minor fixed costs.
  const NODES: usize = 200_000;
  const CHAINS: usize = 2048;

  let mut single = build_heap(1, NODES, CHAINS);

  // Choose a moderate parallelism level to keep the benchmark stable on shared CI hosts.
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4)
    .min(8);
  let mut parallel = build_heap(threads, NODES, CHAINS);

  c.bench_function("gc_major_mark_threads_1", |b| {
    b.iter(|| {
      single
        .heap
        .collect_major(&mut single.roots, &mut single.remembered)
        .unwrap();
      black_box(&single.root_slots);
    });
  });

  c.bench_function("gc_major_mark_threads_parallel", |b| {
    b.iter(|| {
      parallel
        .heap
        .collect_major(&mut parallel.roots, &mut parallel.remembered)
        .unwrap();
      black_box(&parallel.root_slots);
    });
  });
}

#[cfg(target_os = "linux")]
criterion_group! {
  name = gc_major_mark;
  config = Criterion::default()
    .sample_size(10)
    .warm_up_time(Duration::from_millis(100))
    .measurement_time(Duration::from_secs(3));
  targets = bench_major_gc_mark
}

#[cfg(target_os = "linux")]
criterion_main!(gc_major_mark);

#[cfg(not(target_os = "linux"))]
fn main() {}
