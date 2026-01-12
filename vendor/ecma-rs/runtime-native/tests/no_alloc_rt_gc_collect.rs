#![cfg(all(
  target_os = "linux",
  target_arch = "x86_64",
  runtime_native_has_stackmap_test_artifact
))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
static ALLOC_RECORD_LEN: AtomicUsize = AtomicUsize::new(0);
static ALLOC_SIZES: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];
static ALLOC_ALIGNS: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let idx = ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    if idx < ALLOC_SIZES.len() {
      ALLOC_SIZES[idx].store(layout.size(), Ordering::Relaxed);
      ALLOC_ALIGNS[idx].store(layout.align(), Ordering::Relaxed);
      ALLOC_RECORD_LEN.fetch_max(idx + 1, Ordering::Relaxed);
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let idx = ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    if idx < ALLOC_SIZES.len() {
      ALLOC_SIZES[idx].store(layout.size(), Ordering::Relaxed);
      ALLOC_ALIGNS[idx].store(layout.align(), Ordering::Relaxed);
      ALLOC_RECORD_LEN.fetch_max(idx + 1, Ordering::Relaxed);
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let idx = ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    if idx < ALLOC_SIZES.len() {
      ALLOC_SIZES[idx].store(new_size, Ordering::Relaxed);
      ALLOC_ALIGNS[idx].store(layout.align(), Ordering::Relaxed);
      ALLOC_RECORD_LEN.fetch_max(idx + 1, Ordering::Relaxed);
    }
    System.realloc(ptr, layout, new_size)
  }
}

fn assert_stackmap_test_has_gc_pairs() {
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
}

fn trigger_gc_via_stackmap_test() {
  // Triggers `safepoint` above, which calls into `rt_gc_collect`.
  let mut obj = 0u64;
  let ptr = core::ptr::addr_of_mut!(obj).cast::<u8>();
  let ret = unsafe { test_fn(ptr) };
  assert_eq!(ret, ptr);
}

fn assert_rt_gc_collect_does_not_allocate_after_thread_init(with_card_tables: bool) {
  let _rt = TestRuntimeGuard::new();

  // Registering a thread should eagerly parse and index stackmaps so stop-the-world GC doesn't do
  // any lazy allocation work while the world is stopped.
  rt_thread_init(3);

  // Sanity: the stackmap-test artifact must contain at least one statepoint record with a non-zero
  // gc-pair count. This is required for this test to catch any accidental Vec allocations in
  // per-frame root enumeration.
  assert_stackmap_test_has_gc_pairs();

  if with_card_tables {
    // Install a couple of per-object card tables in the old generation before the measured
    // section.
    //
    // Without care, `rt_gc_collect` will try to `reserve` additional capacity for the card-table
    // registry *during* GC, which would call the Rust global allocator while the world is stopped.
    let ptr_size = core::mem::size_of::<*mut u8>();
    let ptr_elem_size = runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG | ptr_size;
    let len = (runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE / ptr_size) + 1;

    let a = runtime_native::rt_alloc_array(len, ptr_elem_size);
    let b = runtime_native::rt_alloc_array(len, ptr_elem_size);
    assert!(!a.is_null());
    assert!(!b.is_null());
  }

  // Ensure the safepoint coordinator singleton is initialized outside the measured section.
  let _ = runtime_native::threading::safepoint::threads_waiting_at_safepoint();

  ALLOC_CALLS.store(0, Ordering::SeqCst);
  ALLOC_RECORD_LEN.store(0, Ordering::SeqCst);

  trigger_gc_via_stackmap_test();

  let allocs = ALLOC_CALLS.load(Ordering::SeqCst);
  if allocs != 0 {
    let n = ALLOC_RECORD_LEN.load(Ordering::SeqCst).min(ALLOC_SIZES.len());
    eprintln!("no_alloc_rt_gc_collect: saw {allocs} allocations; first {n} layouts:");
    for i in 0..n {
      let size = ALLOC_SIZES[i].load(Ordering::SeqCst);
      let align = ALLOC_ALIGNS[i].load(Ordering::SeqCst);
      eprintln!("  #{i}: size={size} align={align}");
    }
  }
  assert_eq!(
    allocs, 0,
    "rt_gc_collect performed unexpected allocations after thread init (with_card_tables={with_card_tables}, alloc calls={allocs})"
  );

  rt_thread_deinit();
}

#[test]
fn rt_gc_collect_does_not_allocate_after_thread_init() {
  // This test installs a global allocator that counts allocations. The libtest harness itself
  // performs allocations when reporting per-test results (e.g. pretty formatting test names), and
  // the harness runs tests in parallel by default.
  //
  // Keep a single `#[test]` in this binary and run both scenarios sequentially under a lock so we
  // do not race with harness bookkeeping allocations.
  let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

  assert_rt_gc_collect_does_not_allocate_after_thread_init(false);
  assert_rt_gc_collect_does_not_allocate_after_thread_init(true);
}
