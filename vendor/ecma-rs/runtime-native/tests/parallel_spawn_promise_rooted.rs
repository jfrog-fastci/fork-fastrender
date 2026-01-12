use runtime_native::abi::PromiseRef;
use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;
use runtime_native::PromiseLayout;
use runtime_native::TypeDescriptor;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

const MAGIC: u64 = 0x0123_4567_89AB_CDEF;

const HEADER_SIZE: usize = std::mem::size_of::<ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;
const SEEN_OFFSET: usize = HEADER_SIZE + std::mem::size_of::<u64>();

static NO_PTR_OFFSETS: [u32; 0] = [];
static TEST_OBJ_DESC: TypeDescriptor = TypeDescriptor::new(
  HEADER_SIZE + std::mem::size_of::<u64>() + std::mem::size_of::<AtomicU64>(),
  &NO_PTR_OFFSETS,
);

unsafe fn init_test_obj(heap: &mut GcHeap) -> *mut u8 {
  let obj = heap.alloc_young(&TEST_OBJ_DESC);
  (obj.add(MAGIC_OFFSET) as *mut u64).write(MAGIC);
  (obj.add(SEEN_OFFSET) as *mut AtomicU64).write(AtomicU64::new(0));
  obj
}

unsafe fn seen_magic_slot(obj: *mut u8) -> &'static AtomicU64 {
  &*(obj.add(SEEN_OFFSET) as *const AtomicU64)
}

extern "C" fn record_magic_and_fulfill(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let magic = (data.add(MAGIC_OFFSET) as *const u64).read();
    let seen = &*(data.add(SEEN_OFFSET) as *const AtomicU64);
    seen.store(magic, Ordering::Release);
    runtime_native::rt_promise_fulfill(promise);
  }
}

fn collect_major(heap: &mut GcHeap) {
  let mut roots = GlobalRootSet::new();
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_major(&mut roots, &mut remembered).unwrap();
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
      runtime_native::rt_parallel_join(self.tasks.as_ptr(), self.tasks.len());
      self.tasks.clear();
    }
  }
}

#[test]
fn parallel_spawn_promise_rooted_roots_and_relocates_task_context() {
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
    .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

  // Ensure worker threads are registered before we try to saturate them with blocking tasks.
  let deadline = Instant::now() + Duration::from_secs(2);
  while runtime_native::threading::thread_counts().worker < workers {
    assert!(Instant::now() < deadline, "worker threads did not register in time");
    std::thread::yield_now();
  }

  let ctx: &'static BlockCtx = Box::leak(Box::new(BlockCtx {
    started: AtomicUsize::new(0),
    release_lock: Mutex::new(false),
    release_cv: Condvar::new(),
  }));

  let mut tasks: Vec<runtime_native::abi::TaskId> = Vec::with_capacity(workers);
  for _ in 0..workers {
    tasks.push(runtime_native::rt_parallel_spawn(
      blocking_task,
      ctx as *const BlockCtx as *mut u8,
    ));
  }

  let deadline = Instant::now() + Duration::from_secs(2);
  while ctx.started.load(Ordering::Acquire) < workers {
    assert!(Instant::now() < deadline, "worker threads did not start blocking tasks in time");
    std::thread::yield_now();
  }

  let join_guard = ParallelJoinGuard { ctx, tasks };

  let mut heap = GcHeap::new();
  let obj_rooted = unsafe { init_test_obj(&mut heap) };
  let obj_rooted_h = unsafe { init_test_obj(&mut heap) };

  let weak_rooted = runtime_native::rt_weak_add(obj_rooted);
  let weak_rooted_h = runtime_native::rt_weak_add(obj_rooted_h);
  let _weak_guard_rooted = WeakHandleGuard(weak_rooted);
  let _weak_guard_rooted_h = WeakHandleGuard(weak_rooted_h);

  let promise_rooted = runtime_native::rt_parallel_spawn_promise_rooted(
    record_magic_and_fulfill,
    obj_rooted,
    PromiseLayout::of::<()>(),
  );

  let mut slot = obj_rooted_h;
  let promise_rooted_h = unsafe {
    runtime_native::rt_parallel_spawn_promise_rooted_h(
      record_magic_and_fulfill,
      runtime_native::roots::handle_from_slot(&mut slot),
      PromiseLayout::of::<()>(),
    )
  };

  // Move/collect while the task is still queued behind the blocking tasks.
  collect_major(&mut heap);

  let after_gc_rooted = runtime_native::rt_weak_get(weak_rooted);
  assert!(!after_gc_rooted.is_null());
  assert!(!heap.is_in_nursery(after_gc_rooted));

  let after_gc_rooted_h = runtime_native::rt_weak_get(weak_rooted_h);
  assert!(!after_gc_rooted_h.is_null());
  assert!(!heap.is_in_nursery(after_gc_rooted_h));

  // Release workers so the rooted task can run.
  {
    let mut guard = ctx.release_lock.lock().unwrap();
    *guard = true;
    ctx.release_cv.notify_all();
  }

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let ptr_rooted = runtime_native::rt_weak_get(weak_rooted);
    let ptr_rooted_h = runtime_native::rt_weak_get(weak_rooted_h);
    assert!(!ptr_rooted.is_null());
    assert!(!ptr_rooted_h.is_null());

    let seen_rooted = unsafe { seen_magic_slot(ptr_rooted) }.load(Ordering::Acquire);
    let seen_rooted_h = unsafe { seen_magic_slot(ptr_rooted_h) }.load(Ordering::Acquire);
    if seen_rooted != 0 && seen_rooted_h != 0 {
      assert_eq!(seen_rooted, MAGIC);
      assert_eq!(seen_rooted_h, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "rooted promise task did not run in time");
    std::thread::yield_now();
  }

  // Ensure the promises are fulfilled.
  let promise_header_rooted = promise_rooted.0.cast::<runtime_native::async_abi::PromiseHeader>();
  let promise_header_rooted_h = promise_rooted_h.0.cast::<runtime_native::async_abi::PromiseHeader>();
  assert!(!promise_header_rooted.is_null());
  assert!(!promise_header_rooted_h.is_null());
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let state_rooted = unsafe { &*promise_header_rooted }.state.load(Ordering::Acquire);
    let state_rooted_h = unsafe { &*promise_header_rooted_h }.state.load(Ordering::Acquire);
    if state_rooted == runtime_native::async_abi::PromiseHeader::FULFILLED
      && state_rooted_h == runtime_native::async_abi::PromiseHeader::FULFILLED
    {
      break;
    }
    if state_rooted == runtime_native::async_abi::PromiseHeader::REJECTED {
      panic!("expected rooted promise task to fulfill, but it rejected");
    }
    if state_rooted_h == runtime_native::async_abi::PromiseHeader::REJECTED {
      panic!("expected rooted-h promise task to fulfill, but it rejected");
    }
    assert!(Instant::now() < deadline, "promise did not settle in time");
    runtime_native::rt_async_poll();
    std::thread::yield_now();
  }

  // After the task executes, the root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak_rooted).is_null() && runtime_native::rt_weak_get(weak_rooted_h).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted promise tasks executed (root not released?)"
    );
  }

  // Join tasks in Drop.
  drop(join_guard);
}
