use runtime_native::abi::PromiseRef;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[repr(C, align(32))]
struct Payload([u8; 1024]);

const PAYLOAD_MAGIC: u64 = 0x0123_4567_89AB_CDEF;
const TIMEOUT: Duration = Duration::from_secs(if cfg!(debug_assertions) { 30 } else { 10 });

extern "C" fn write_payload_and_fulfill(_data: *mut u8, promise: PromiseRef) {
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise);
    assert!(!payload.is_null());
    assert_eq!(
      payload as usize % core::mem::align_of::<Payload>(),
      0,
      "payload pointer must respect PromiseLayout.align"
    );
    (payload as *mut u64).write(PAYLOAD_MAGIC);
    runtime_native::rt_promise_fulfill(promise);
  }
}

#[repr(C)]
struct BlockCtx {
  started: AtomicUsize,
  release_lock: Mutex<bool>,
  release_cv: Condvar,
}

extern "C" fn blocking_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const BlockCtx) };
  ctx.started.fetch_add(1, Ordering::Release);

  // Avoid deadlocking stop-the-world GC: treat this worker as GC-safe while blocked on the condvar.
  let gc_safe = runtime_native::threading::enter_gc_safe_region();

  let mut guard = ctx.release_lock.lock().unwrap();
  while !*guard {
    guard = ctx.release_cv.wait(guard).unwrap();
  }
  drop(guard);
  drop(gc_safe);
}

struct ParallelJoinGuard {
  ctx: *mut BlockCtx,
  tasks: Vec<runtime_native::abi::TaskId>,
}

impl Drop for ParallelJoinGuard {
  fn drop(&mut self) {
    unsafe {
      let ctx = &*self.ctx;
      {
        let mut guard = ctx.release_lock.lock().unwrap();
        *guard = true;
        ctx.release_cv.notify_all();
      }
    }

    if !self.tasks.is_empty() {
      runtime_native::rt_parallel_join(self.tasks.as_ptr(), self.tasks.len());
      self.tasks.clear();
    }

    if !self.ctx.is_null() {
      unsafe {
        drop(Box::from_raw(self.ctx));
      }
      self.ctx = core::ptr::null_mut();
    }
  }
}

struct WeakHandleGuard(u64);

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

#[test]
fn parallel_spawn_promise_payload_promise_is_gc_managed_and_relocates() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the global worker pool is initialized.
  extern "C" fn noop(_data: *mut u8) {}
  let warmup = runtime_native::rt_parallel_spawn(noop, core::ptr::null_mut());
  runtime_native::rt_parallel_join(&warmup as *const runtime_native::abi::TaskId, 1);

  // Match the runtime's worker-count selection logic.
  let workers = std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS")
    .ok()
    .or_else(|| std::env::var("RT_NUM_THREADS").ok())
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|&n| n > 0)
    .unwrap_or_else(|| {
      let default = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
      if cfg!(debug_assertions) {
        default.min(32)
      } else {
        default
      }
    });

  // Ensure worker threads are registered before saturating them with blocking tasks.
  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::threading::thread_counts().worker < workers {
    assert!(Instant::now() < deadline, "worker threads did not register in time");
    std::thread::yield_now();
  }

  let ctx: *mut BlockCtx = Box::into_raw(Box::new(BlockCtx {
    started: AtomicUsize::new(0),
    release_lock: Mutex::new(false),
    release_cv: Condvar::new(),
  }));

  let mut tasks: Vec<runtime_native::abi::TaskId> = Vec::with_capacity(workers);
  for _ in 0..workers {
    tasks.push(runtime_native::rt_parallel_spawn(blocking_task, ctx.cast::<u8>()));
  }

  let deadline = Instant::now() + TIMEOUT;
  while unsafe { &*ctx }.started.load(Ordering::Acquire) < workers {
    assert!(Instant::now() < deadline, "worker threads did not start blocking tasks in time");
    std::thread::yield_now();
  }

  let _join_guard = ParallelJoinGuard { ctx, tasks };

  // Ensure the next promise allocation lands in the nursery so a forced minor GC can relocate it.
  runtime_native::rt_gc_collect_minor();

  let base_handles = runtime_native::roots::global_persistent_handle_table().live_count();

  let promise = runtime_native::rt_parallel_spawn_promise(
    write_payload_and_fulfill,
    core::ptr::null_mut(),
    PromiseLayout::of::<Payload>(),
  );
  assert!(!promise.is_null());

  // Root the promise via an addressable slot so minor GC can update it in-place.
  let mut promise_root: usize = promise.0 as usize;
  runtime_native::rt_global_root_register(&mut promise_root as *mut usize);

  // The queued wrapper task roots the promise via one persistent handle.
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_handles + 1
  );

  // Store the "before" pointer in a Rust heap allocation so a conservative stack scan in debug
  // builds cannot rewrite it (see `tests/gc_collect_minor.rs` for background).
  let initial_ptr = Box::new(promise_root as usize);
  assert_ne!(*initial_ptr, 0);

  // The promise should be allocated in the nursery so minor GC can evacuate it.
  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(
    (*initial_ptr) >= (young_start as usize) && (*initial_ptr) < (young_end as usize),
    "expected payload promise to be allocated in the nursery; got ptr={:p}, young={young_start:p}..{young_end:p}",
    *initial_ptr as *mut u8
  );

  // Nursery evacuation must be able to relocate the promise object while it is still pending/queued.
  runtime_native::rt_gc_collect_minor();

  // The GC may conservatively scan and mutate stack slots that look like GC pointers in debug
  // builds when stackmaps are missing for Rust frames. Re-read the young range after collection so
  // we don't compare against stack-scribbled locals.
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  let after_minor_ptr = promise_root as *mut u8;
  assert!(!after_minor_ptr.is_null());
  assert_ne!(
    *initial_ptr as *mut u8, after_minor_ptr,
    "payload promise should relocate under minor GC (GC-managed object)"
  );
  assert!(
    (after_minor_ptr as usize) < (young_start as usize) || (after_minor_ptr as usize) >= (young_end as usize),
    "after minor GC, payload promise should have been evacuated out of the nursery; got ptr={after_minor_ptr:p}, young={young_start:p}..{young_end:p}"
  );

  // Release the blocking tasks so the payload promise task can run (it should execute *after* the GC
  // relocation above).
  unsafe {
    let ctx_ref = &*ctx;
    {
      let mut guard = ctx_ref.release_lock.lock().unwrap();
      *guard = true;
      ctx_ref.release_cv.notify_all();
    }
  }

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let p_ptr = promise_root as *mut u8;
    assert!(!p_ptr.is_null());
    let hdr = p_ptr.cast::<PromiseHeader>();
    let state = unsafe { &*hdr }.state.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED {
      let payload_ptr = runtime_native::rt_promise_payload_ptr(PromiseRef(p_ptr.cast()));
      assert!(!payload_ptr.is_null());
      assert_eq!(payload_ptr as usize % core::mem::align_of::<Payload>(), 0);
      let got = unsafe { (payload_ptr as *const u64).read() };
      assert_eq!(got, PAYLOAD_MAGIC);
      break;
    }
    if state == PromiseHeader::REJECTED {
      panic!("expected payload promise to fulfill, but it rejected");
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for payload promise to settle"
    );
    runtime_native::rt_async_poll_legacy();
    std::thread::yield_now();
  }

  // Ensure the detached worker task has released its persistent handle root before we attempt to
  // collect the promise.
  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::roots::global_persistent_handle_table().live_count() != base_handles {
    assert!(
      Instant::now() < deadline,
      "promise task did not release its persistent handle after completion"
    );
    std::thread::yield_now();
  }

  let weak = runtime_native::rt_weak_add(promise_root as *mut u8);
  let _weak_guard = WeakHandleGuard(weak);

  runtime_native::rt_global_root_unregister(&mut promise_root as *mut usize);

  // After dropping the last strong root, the promise should become collectible.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    runtime_native::rt_gc_collect_major();
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for payload promise to be collected after dropping roots"
    );
    std::thread::yield_now();
  }
}
