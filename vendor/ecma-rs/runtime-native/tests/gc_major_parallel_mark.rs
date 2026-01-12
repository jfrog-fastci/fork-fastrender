use std::mem;
use std::ptr;

use runtime_native::gc::{HeapConfig, HeapLimits, ObjHeader, RootStack, SimpleRememberedSet, TypeDescriptor};
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
  value: usize,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[test]
fn major_gc_parallel_mark_keeps_live_and_collects_dead() {
  let _rt = TestRuntimeGuard::new();

  // Force the parallel marking path regardless of host defaults.
  let config = HeapConfig {
    major_gc_mark_threads: 4,
    ..HeapConfig::default()
  };
  let limits = HeapLimits::default();

  let mut heap = GcHeap::with_config(config, limits);
  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  const LIVE: usize = 50_000;
  const DEAD: usize = 50_000;
  const ROOTS: usize = 512;

  // Build a forest of independent live chains to ensure the parallel marker actually has work to
  // distribute across multiple threads.
  let roots_count = ROOTS.max(1).min(LIVE.max(1));
  let base = LIVE / roots_count;
  let mut extra = LIVE % roots_count;
  let mut next_value = 0usize;

  let mut root_slots: Vec<Box<*mut u8>> = Vec::with_capacity(roots_count);
  let mut chain_lens: Vec<usize> = Vec::with_capacity(roots_count);
  let mut live_handles = Vec::with_capacity(LIVE);

  for _ in 0..roots_count {
    let mut chain_len = base;
    if extra > 0 {
      chain_len += 1;
      extra -= 1;
    }
    if chain_len == 0 {
      continue;
    }
    chain_lens.push(chain_len);

    let mut slot = Box::new(ptr::null_mut());
    *slot = heap.alloc_old(&NODE_DESC);
    roots.push(&mut *slot as *mut *mut u8);

    unsafe {
      let n = &mut *(*slot as *mut Node);
      n.next = ptr::null_mut();
      n.value = next_value;
    }
    next_value += 1;
    live_handles.push(heap.weak_add(*slot));

    let mut prev = *slot;
    for _ in 1..chain_len {
      let obj = heap.alloc_old(&NODE_DESC);
      unsafe {
        let n = &mut *(obj as *mut Node);
        n.next = ptr::null_mut();
        n.value = next_value;
        (*(prev as *mut Node)).next = obj;
      }
      next_value += 1;
      live_handles.push(heap.weak_add(obj));
      prev = obj;
    }
    root_slots.push(slot);
  }
  assert_eq!(next_value, LIVE);

  // Allocate a large dead set and keep only weak handles.
  let mut dead_handles = Vec::with_capacity(DEAD);
  for i in 0..DEAD {
    let obj = heap.alloc_old(&NODE_DESC);
    unsafe {
      let n = &mut *(obj as *mut Node);
      n.next = ptr::null_mut();
      n.value = 0xD00D_0000 + i;
    }
    dead_handles.push(heap.weak_add(obj));
  }

  heap.collect_major(&mut roots, &mut remembered).unwrap();

  // Live objects must remain readable and correctly linked.
  let mut expected_value = 0usize;
  for (slot, &chain_len) in root_slots.iter().zip(chain_lens.iter()) {
    let mut cur = **slot;
    for _ in 0..chain_len {
      assert!(!cur.is_null());
      unsafe {
        let n = &*(cur as *const Node);
        assert_eq!(n.value, expected_value);
        cur = n.next;
      }
      expected_value += 1;
    }
    assert!(cur.is_null());
  }
  assert_eq!(expected_value, LIVE);

  // Weak handles for live objects must still resolve.
  for h in live_handles {
    assert!(heap.weak_get(h).is_some());
  }

  // Weak handles for dead objects must be cleared.
  for h in dead_handles {
    assert!(heap.weak_get(h).is_none());
  }
}
