use std::mem;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

#[repr(C)]
struct Node {
  header: ObjHeader,
  left: *mut u8,
  right: *mut u8,
  value: usize,
}

const NODE_PTR_OFFSETS: [u32; 2] = [
  mem::offset_of!(Node, left) as u32,
  mem::offset_of!(Node, right) as u32,
];

static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

fn build_heap_graph(heap: &mut GcHeap, nodes: usize) -> Vec<*mut u8> {
  assert!(nodes >= 4);

  let mut objs: Vec<*mut u8> = Vec::with_capacity(nodes);
  for i in 0..nodes {
    let obj = heap.alloc_old(&NODE_DESC);
    // Initialize payload (not used by the GC but helps ensure the object looks "real").
    unsafe {
      (*(obj as *mut Node)).left = core::ptr::null_mut();
      (*(obj as *mut Node)).right = core::ptr::null_mut();
      (*(obj as *mut Node)).value = i;
    }
    objs.push(obj);
  }

  let half = nodes / 2;

  // Create two disjoint strongly-connected graphs (0..half) and (half..nodes). Root only the
  // first half so the second half is unreachable and must be reclaimed.
  for i in 0..half {
    let a = objs[i];
    let left = objs[(i + 1) % half];
    let right = objs[(i + 3) % half];
    unsafe {
      (*(a as *mut Node)).left = left;
      (*(a as *mut Node)).right = right;
    }
  }
  for i in half..nodes {
    let a = objs[i];
    let local = i - half;
    let left = objs[half + ((local + 1) % (nodes - half))];
    let right = objs[half + ((local + 5) % (nodes - half))];
    unsafe {
      (*(a as *mut Node)).left = left;
      (*(a as *mut Node)).right = right;
    }
  }

  objs
}

fn run_and_capture_live_set(mark_workers: usize) -> Vec<bool> {
  let mut heap = GcHeap::new();
  let objs = build_heap_graph(&mut heap, 4096);

  let mut remembered = NullRememberedSet::default();

  // Root the first object (graph 0..half).
  let mut root = objs[0];
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);

  heap
    .collect_major_with_mark_workers(&mut roots, &mut remembered, mark_workers)
    .unwrap();

  let live: Vec<bool> = objs.iter().map(|&obj| heap.debug_is_marked(obj)).collect();

  // Sanity: ensure liveness matches the intended heap graph.
  let half = objs.len() / 2;
  assert!(live[..half].iter().all(|&b| b), "expected graph A to be live");
  assert!(
    live[half..].iter().all(|&b| !b),
    "expected graph B to be unreachable"
  );

  live
}

#[test]
fn parallel_major_marker_matches_single_threaded_live_set() {
  let _rt = TestRuntimeGuard::new();

  let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
  let parallel_workers = available.min(4).max(1);

  let single = run_and_capture_live_set(1);
  let parallel = run_and_capture_live_set(parallel_workers);

  assert_eq!(single, parallel);
}

