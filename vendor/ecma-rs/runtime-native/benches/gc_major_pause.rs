// Benchmark: stop-the-world major GC pause time.
//
// This is intended to capture improvements from parallelizing the major GC marking phase.

#[cfg(target_os = "linux")]
use std::sync::Once;
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
use runtime_native::gc::ObjHeader;
#[cfg(target_os = "linux")]
use runtime_native::gc::RememberedSet;
#[cfg(target_os = "linux")]
use runtime_native::gc::RootStack;
#[cfg(target_os = "linux")]
use runtime_native::gc::TypeDescriptor;
#[cfg(target_os = "linux")]
use runtime_native::GcHeap;

#[cfg(target_os = "linux")]
#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
  payload: [u8; 16],
}

#[cfg(target_os = "linux")]
const NODE_PTR_OFFSETS: [u32; 1] = [std::mem::offset_of!(Node, next) as u32];

#[cfg(target_os = "linux")]
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(std::mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[cfg(target_os = "linux")]
#[derive(Default)]
struct NullRememberedSet;

#[cfg(target_os = "linux")]
impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[cfg(target_os = "linux")]
fn init() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| {
    // Force any one-time initialization work (thread pools, etc.) to happen outside the measured
    // benchmark loops.
    let mut heap = GcHeap::new();
    let mut roots = RootStack::new();
    let mut remembered = NullRememberedSet::default();
    let _ = heap.collect_major(&mut roots, &mut remembered);
  });
}

#[cfg(target_os = "linux")]
fn build_heap_with_live_list(nodes: usize) -> (GcHeap, RootStack, NullRememberedSet) {
  let mut heap = GcHeap::new();

  let mut objs: Vec<*mut u8> = Vec::with_capacity(nodes);
  for _ in 0..nodes {
    objs.push(heap.alloc_old(&NODE_DESC));
  }
  for i in 0..nodes {
    let next = if i + 1 < nodes { objs[i + 1] } else { core::ptr::null_mut() };
    unsafe {
      (*(objs[i] as *mut Node)).next = next;
    }
  }

  let mut root = objs[0];
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);

  (heap, roots, NullRememberedSet::default())
}

#[cfg(target_os = "linux")]
fn gc_major_pause_parallel(c: &mut Criterion) {
  init();

  // Large enough to make marking dominate, but small enough to keep the benchmark fast on CI hosts.
  const NODES: usize = 200_000;
  let (mut heap, mut roots, mut remembered) = build_heap_with_live_list(NODES);

  c.bench_function("gc_major_pause_parallel", |b| {
    b.iter(|| {
      heap.collect_major(&mut roots, &mut remembered).unwrap();
      black_box(heap.stats().last_major_pause);
    });
  });
}

#[cfg(target_os = "linux")]
fn gc_major_pause_single_worker(c: &mut Criterion) {
  init();

  const NODES: usize = 200_000;
  let (mut heap, mut roots, mut remembered) = build_heap_with_live_list(NODES);

  c.bench_function("gc_major_pause_single_worker", |b| {
    b.iter(|| {
      heap
        .collect_major_with_mark_workers(&mut roots, &mut remembered, 1)
        .unwrap();
      black_box(heap.stats().last_major_pause);
    });
  });
}

#[cfg(target_os = "linux")]
criterion_group! {
  name = gc_major_pause;
  config = Criterion::default()
    .sample_size(10)
    .warm_up_time(Duration::from_millis(100))
    .measurement_time(Duration::from_millis(300));
  targets = gc_major_pause_single_worker, gc_major_pause_parallel
}

#[cfg(target_os = "linux")]
criterion_main!(gc_major_pause);

#[cfg(not(target_os = "linux"))]
fn main() {}

