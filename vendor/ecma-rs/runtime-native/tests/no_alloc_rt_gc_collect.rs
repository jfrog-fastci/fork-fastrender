#![cfg(all(
  target_os = "linux",
  target_arch = "x86_64",
  runtime_native_has_stackmap_test_artifact
))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::statepoints::StatepointRecord;
use runtime_native::{rt_thread_deinit, rt_thread_init};

include!(env!("RUNTIME_NATIVE_STACKMAP_TEST_DATA_RS"));

extern "C" {
  fn test_fn(p: *mut u8) -> *mut u8;
}

// Override the weak `safepoint` symbol from `build.rs`' generated stackmap test module.
//
// `test_fn` contains an LLVM statepoint. By triggering `rt_gc_collect` from inside the `safepoint`
// callee, the GC initiator must recover the managed callsite cursor by walking the frame-pointer
// chain. This ensures stop-the-world root enumeration sees at least one statepoint record with
// `gc_pair_count > 0`.
core::arch::global_asm!(
  r#"
  .text
  .globl safepoint
  .type safepoint,@function
safepoint:
  push rbp
  mov rbp, rsp
  call rt_gc_collect
  pop rbp
  ret
"#
);

struct CountingAlloc;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }
}

#[test]
fn rt_gc_collect_does_not_allocate_after_thread_init() {
  let _rt = TestRuntimeGuard::new();

  // Registering a thread should eagerly parse and index stackmaps so stop-the-world GC doesn't do
  // any lazy allocation work while the world is stopped.
  rt_thread_init(3);

  // Sanity: the stackmap-test artifact must contain at least one statepoint record with a non-zero
  // gc-pair count. This is required for this test to catch any accidental Vec allocations in
  // per-frame root enumeration.
  let expected_ip = (test_fn as usize as u64).wrapping_add(STACKMAP_INSTRUCTION_OFFSET as u64);
  let stackmaps = runtime_native::stackmap::try_stackmaps().expect("expected stackmaps to be available");
  let callsite = stackmaps
    .lookup(expected_ip)
    .expect("expected stackmap_test callsite record to be present in stackmaps");
  let statepoint =
    StatepointRecord::new(callsite.record).expect("expected stackmap_test record to be a statepoint");
  assert!(
    statepoint.gc_pair_count() > 0,
    "stackmap_test artifact unexpectedly has gc_pair_count=0; \
     this would not exercise per-frame stack root enumeration allocation paths"
  );

  // Ensure the safepoint coordinator singleton is initialized outside the measured section.
  let _ = runtime_native::threading::safepoint::threads_waiting_at_safepoint();

  ALLOC_CALLS.store(0, Ordering::SeqCst);

  // Triggers `safepoint` above, which calls into `rt_gc_collect`.
  let mut obj = 0u64;
  let ptr = core::ptr::addr_of_mut!(obj).cast::<u8>();
  let ret = unsafe { test_fn(ptr) };
  assert_eq!(ret, ptr);

  let allocs = ALLOC_CALLS.load(Ordering::SeqCst);
  assert_eq!(
    allocs, 0,
    "rt_gc_collect performed unexpected allocations after thread init (alloc calls={allocs})"
  );

  rt_thread_deinit();
}
