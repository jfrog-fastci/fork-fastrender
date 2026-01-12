use runtime_native::abi::{PromiseRef, RtShapeDescriptor, RtShapeId};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::gc::ObjHeader;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_gc_collect_major, rt_gc_collect_minor, rt_gc_get_young_range, rt_gc_register_root_slot, rt_gc_root_get,
  rt_gc_unregister_root_slot,
};
use runtime_native::{rt_parallel_join, rt_parallel_spawn, rt_thread_deinit, rt_thread_init, shape_table};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

// Stop-the-world coordination and worker-pool startup can take much longer in debug builds under
// parallel CI execution; keep release builds strict but give debug builds enough slack to avoid
// flaky timeouts.
const TIMEOUT: Duration = if cfg!(debug_assertions) {
  Duration::from_secs(30)
} else {
  Duration::from_secs(2)
};

#[repr(C)]
struct Leaf {
  _header: ObjHeader,
  _value: u64,
}

#[repr(C)]
struct PromisePayload {
  ptr: *mut u8,
}

/// GC-managed `Promise<T>` layout: `PromiseHeader` prefix + inline payload.
#[repr(C)]
struct PromiseWithPtrPayload {
  header: PromiseHeader,
  payload: PromisePayload,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static PROMISE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(PromiseWithPtrPayload, payload) as u32];
    static SHAPES: [RtShapeDescriptor; 2] = [
      // Shape 1: leaf object (no pointers).
      RtShapeDescriptor {
        size: mem::size_of::<Leaf>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      // Shape 2: `PromiseHeader + { ptr: GcPtr }` (payload pointer field is traceable).
      RtShapeDescriptor {
        size: mem::size_of::<PromiseWithPtrPayload>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: PROMISE_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: PROMISE_PTR_OFFSETS.len() as u32,
        reserved: 0,
      },
    ];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn nursery_contains(ptr: *mut u8) -> bool {
  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  // SAFETY: out pointers are valid.
  unsafe { rt_gc_get_young_range(&mut start, &mut end) };
  let addr = ptr as usize;
  addr >= start as usize && addr < end as usize
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

#[repr(C)]
struct BlockCtx {
  started: AtomicUsize,
  release_lock: Mutex<bool>,
  release_cv: Condvar,
}

extern "C" fn blocking_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const BlockCtx) };
  ctx.started.fetch_add(1, Ordering::Release);

  // Block worker threads in a GC-safe region so a stop-the-world GC doesn't need to scan their
  // stacks/registers for roots while we're trying to keep the promise task queued.
  let gc_safe = runtime_native::threading::enter_gc_safe_region();

  let mut guard = ctx.release_lock.lock().unwrap();
  while !*guard {
    guard = ctx.release_cv.wait(guard).unwrap();
  }
  drop(guard);
  drop(gc_safe);
}

struct ParallelJoinGuard {
  ctx: &'static BlockCtx,
  tasks: Vec<runtime_native::abi::TaskId>,
}

impl Drop for ParallelJoinGuard {
  fn drop(&mut self) {
    {
      let mut guard = self.ctx.release_lock.lock().unwrap();
      *guard = true;
      self.ctx.release_cv.notify_all();
    }
    if !self.tasks.is_empty() {
      rt_parallel_join(self.tasks.as_ptr(), self.tasks.len());
      self.tasks.clear();
    }
  }
}

struct TaskData {
  weak_tx: mpsc::Sender<(u64, u64)>,
}

extern "C" fn write_gc_ptr_payload_and_fulfill(data: *mut u8, promise: PromiseRef) {
  // Safety: `data` is allocated as a `Box<TaskData>` in the test and is owned by this callback.
  let data = unsafe { Box::from_raw(data as *mut TaskData) };

  // Allocate a pinned object so conservative stack scanning (debug fallback) can't keep it alive by
  // mistake. If the promise payload pointer is not traced by GC, a major GC should collect it and
  // clear the weak handle.
  let obj = runtime_native::rt_alloc_pinned(mem::size_of::<Leaf>(), RtShapeId(1));
  let weak = runtime_native::rt_weak_add(obj);

  // Negative control: an unreferenced pinned object should be collected by a forced major GC.
  let garbage = runtime_native::rt_alloc_pinned(mem::size_of::<Leaf>(), RtShapeId(1));
  let weak_garbage = runtime_native::rt_weak_add(garbage);

  // Best effort: if the main thread panics early (test failure), it may drop the receiver before
  // this worker runs. Avoid panicking across the `extern "C"` boundary.
  let _ = data.weak_tx.send((weak, weak_garbage));

  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise) as *mut PromisePayload;
    if payload.is_null() {
      // We cannot unwind across the FFI boundary; abort so the failure is visible.
      std::process::abort();
    }
    (*payload).ptr = obj;
    runtime_native::rt_promise_fulfill(promise);
  }
  // `data` dropped here.
}

#[test]
fn parallel_spawn_promise_with_shape_traces_payload_pointers() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  // Ensure the global worker pool is initialized.
  extern "C" fn noop(_data: *mut u8) {}
  let warmup = rt_parallel_spawn(noop, core::ptr::null_mut());
  rt_parallel_join(&warmup as *const runtime_native::abi::TaskId, 1);

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

  // Ensure worker threads are registered before we try to saturate them.
  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::threading::thread_counts().worker < workers {
    assert!(Instant::now() < deadline, "worker threads did not register in time");
    std::thread::yield_now();
  }

  // Saturate the worker pool so the promise task is queued (and retains its promise handle in
  // runtime-owned state that must be GC-safe).
  let ctx: &'static BlockCtx = Box::leak(Box::new(BlockCtx {
    started: AtomicUsize::new(0),
    release_lock: Mutex::new(false),
    release_cv: Condvar::new(),
  }));
  let mut tasks = Vec::with_capacity(workers);
  for _ in 0..workers {
    tasks.push(rt_parallel_spawn(
      blocking_task,
      ctx as *const BlockCtx as *mut u8,
    ));
  }

  let deadline = Instant::now() + TIMEOUT;
  while ctx.started.load(Ordering::Acquire) < workers {
    assert!(Instant::now() < deadline, "worker threads did not start blocking tasks in time");
    std::thread::yield_now();
  }

  let join_guard = ParallelJoinGuard { ctx, tasks };

  let (weak_tx, weak_rx) = mpsc::channel::<(u64, u64)>();
  let data = Box::new(TaskData { weak_tx });

  let promise = runtime_native::rt_parallel_spawn_promise_with_shape(
    write_gc_ptr_payload_and_fulfill,
    Box::into_raw(data) as *mut u8,
    mem::size_of::<PromiseWithPtrPayload>(),
    mem::align_of::<PromiseWithPtrPayload>(),
    RtShapeId(2),
  );
  assert!(!promise.is_null());

  // Root the promise for the remainder of the test so it is the sole strong root retaining the
  // worker-allocated `obj`.
  let mut promise_root: *mut u8 = promise.0.cast();
  let promise_root_handle = rt_gc_register_root_slot(&mut promise_root as *mut *mut u8);

  // Force a stop-the-world GC while the promise task is still queued. If the runtime stored a raw
  // `PromiseRef` in its internal work item, it would become stale after relocation.
  assert!(
    nursery_contains(promise_root),
    "expected promise to be nursery-allocated so minor GC relocation is exercised"
  );

  // In debug builds, the runtime can fall back to conservative stack scanning when stackmaps are
  // unavailable, which can update any stack word that looks like a young object pointer (including
  // locals used for test bookkeeping). Tag the pointer so it is not a plausible object-start
  // address.
  let promise_before_tagged = (promise_root as usize) | 1;
  rt_gc_collect_minor();
  let promise_after = rt_gc_root_get(promise_root_handle) as usize;
  assert!(
    !nursery_contains(promise_after as *mut u8),
    "evacuated promise must not remain in nursery"
  );
  let promise_before = promise_before_tagged & !1;
  assert_ne!(
    promise_after, promise_before,
    "expected promise to be relocated by minor GC while queued"
  );

  // Release the saturated worker pool so the promise task can execute.
  drop(join_guard);

  let (weak, weak_garbage) = weak_rx
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|_| panic!("timed out waiting for worker to send weak handle"));
  let _weak_guard = WeakHandleGuard(weak);
  let _weak_garbage_guard = WeakHandleGuard(weak_garbage);

  // Wait for the worker task to settle the promise.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let p = rt_gc_root_get(promise_root_handle);
    assert!(!p.is_null());
    let header = unsafe { &*(p as *const PromiseHeader) };
    match header.load_state() {
      PromiseHeader::PENDING => {
        assert!(Instant::now() < deadline, "timed out waiting for promise settlement");
        std::thread::yield_now();
      }
      PromiseHeader::FULFILLED => break,
      PromiseHeader::REJECTED => panic!("promise unexpectedly rejected"),
      other => panic!("unexpected promise state: {other}"),
    }
  }

  // A major GC should keep `obj` alive solely via the traceable payload pointer slot in the promise.
  rt_gc_collect_major();

  let obj_after = runtime_native::rt_weak_get(weak);
  assert!(
    !obj_after.is_null(),
    "GC should keep obj alive via the traceable promise payload pointer"
  );

  let garbage_after = runtime_native::rt_weak_get(weak_garbage);
  assert!(
    garbage_after.is_null(),
    "GC should collect unreachable objects even if they have weak handles"
  );

  // Ensure the payload slot still points at the object.
  let promise_ptr = rt_gc_root_get(promise_root_handle);
  let payload_ptr = runtime_native::rt_promise_payload_ptr(PromiseRef(promise_ptr.cast()));
  assert!(!payload_ptr.is_null());
  let payload = unsafe { &*(payload_ptr as *const PromisePayload) };
  assert_eq!(payload.ptr, obj_after);

  rt_gc_unregister_root_slot(promise_root_handle);
  rt_thread_deinit();
}
