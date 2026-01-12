use runtime_native::abi::TaskId;
use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;
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

extern "C" fn record_magic(data: *mut u8) {
  unsafe {
    let magic = (data.add(MAGIC_OFFSET) as *const u64).read();
    let seen = &*(data.add(SEEN_OFFSET) as *const AtomicU64);
    seen.store(magic, Ordering::Release);
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

#[test]
fn microtask_rooted_keeps_gc_object_alive_across_gc() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  runtime_native::rt_queue_microtask_rooted(record_magic, obj);

  // Move/collect while the task is still queued.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "microtask did not run in time");
    std::thread::yield_now();
  }

  // After the microtask executes, the root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after microtask executed (root not released?)"
    );
  }
}

#[test]
fn timeout_rooted_keeps_gc_object_alive_across_gc() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  // Schedule with 0 delay so the timer is due immediately once the event loop is polled.
  runtime_native::rt_set_timeout_rooted(record_magic, obj, 0);

  // Move/collect while the timer is still pending.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "timeout did not fire in time");
    std::thread::yield_now();
  }

  // After the timeout fires, the root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after timeout fired (root not released?)"
    );
  }
}

#[test]
fn timeout_rooted_releases_gc_object_when_cleared_before_fire() {
  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  // Use a long delay so the timer will still be pending when we clear it.
  let id = runtime_native::rt_set_timeout_rooted(record_magic, obj, 60_000);
  assert_ne!(id.0, 0);

  // Move/collect while the timer is still pending.
  collect_major(&mut heap);
  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  runtime_native::rt_clear_timer(id);

  // Once cleared, the rooted context must be released.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted timeout was cleared (root not released?)"
    );
    std::thread::yield_now();
  }
}

#[test]
fn interval_rooted_keeps_gc_object_alive_until_cleared() {
  static FIRE_COUNT: AtomicUsize = AtomicUsize::new(0);
  FIRE_COUNT.store(0, Ordering::Release);

  extern "C" fn interval_record_magic(data: *mut u8) {
    record_magic(data);
    FIRE_COUNT.fetch_add(1, Ordering::AcqRel);
  }

  let mut heap = GcHeap::new();
  let _rt = TestRuntimeGuard::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  // Use a 0ms interval so the callback is due immediately once the event loop is polled.
  let id = runtime_native::rt_set_interval_rooted(interval_record_magic, obj, 0);
  assert_ne!(id.0, 0);

  // Move/collect while the interval is still pending.
  collect_major(&mut heap);
  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  // Wait for at least one interval callback to run.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let fired = FIRE_COUNT.load(Ordering::Acquire);
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    if fired > 0 {
      let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "interval did not fire in time");
    std::thread::yield_now();
  }

  // The interval is still registered, so the object should remain alive.
  collect_major(&mut heap);
  assert!(!runtime_native::rt_weak_get(weak).is_null());

  runtime_native::rt_clear_timer(id);

  // After clearing, the root is released and the object can be collected.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted interval was cleared (root not released?)"
    );
    std::thread::yield_now();
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
  tasks: Vec<TaskId>,
  weak: u64,
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

    if self.weak != 0 {
      runtime_native::rt_weak_remove(self.weak);
      self.weak = 0;
    }
  }
}

#[test]
fn parallel_spawn_rooted_roots_and_relocates_task_context() {
  let _rt = TestRuntimeGuard::new();

  // Worker thread startup and scheduling can legitimately take longer on debug/test builds (which
  // are slower, and often run under heavy contention on CI hosts). Keep release builds strict, but
  // avoid flaky timeouts in debug builds.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(2)
  };

  // Ensure the global worker pool is initialized.
  extern "C" fn noop(_data: *mut u8) {}
  let warmup = runtime_native::rt_parallel_spawn(noop, core::ptr::null_mut());
  runtime_native::rt_parallel_join(&warmup as *const TaskId, 1);

  // Match the runtime's worker-count selection logic (`parallel::Scheduler::new`):
  // - honor the `ECMA_RS_RUNTIME_NATIVE_THREADS` env var
  // - accept `RT_NUM_THREADS` as a legacy alias
  // - clamp debug builds to a reasonable maximum to keep CI stable on large machines
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

  // Ensure worker threads are registered before we try to saturate them with blocking tasks.
  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::threading::thread_counts().worker < workers {
    assert!(Instant::now() < deadline, "worker threads did not register in time");
    std::thread::yield_now();
  }

  let ctx: &'static BlockCtx = Box::leak(Box::new(BlockCtx {
    started: AtomicUsize::new(0),
    release_lock: Mutex::new(false),
    release_cv: Condvar::new(),
  }));

  let mut tasks: Vec<TaskId> = Vec::with_capacity(workers + 1);
  for _ in 0..workers {
    tasks.push(runtime_native::rt_parallel_spawn(blocking_task, ctx as *const BlockCtx as *mut u8));
  }

  let deadline = Instant::now() + TIMEOUT;
  while ctx.started.load(Ordering::Acquire) < workers {
    assert!(Instant::now() < deadline, "worker threads did not start blocking tasks in time");
    std::thread::yield_now();
  }

  let mut heap = GcHeap::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);

  let rooted = runtime_native::rt_parallel_spawn_rooted(record_magic, obj);
  tasks.push(rooted);

  let join_guard = ParallelJoinGuard {
    ctx,
    tasks,
    weak,
  };

  // Move/collect while the rooted task is still queued behind the blocking tasks.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  // Release workers so the rooted task can run.
  {
    let mut guard = ctx.release_lock.lock().unwrap();
    *guard = true;
    ctx.release_cv.notify_all();
  }

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "rooted task did not run in time");
    std::thread::yield_now();
  }

  // Once the task completes, its root must be released even if the TaskId is not joined yet.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted task completed (root not released?)"
    );
  }

  // Join tasks and release weak handle in Drop.
  drop(join_guard);
}

#[cfg(unix)]
#[test]
fn io_register_rooted_keeps_gc_object_alive_until_unregistered() {
  use std::os::unix::io::RawFd;

  let _rt = TestRuntimeGuard::new();

  struct FdGuard(RawFd);
  impl Drop for FdGuard {
    fn drop(&mut self) {
      unsafe {
        libc::close(self.0);
      }
    }
  }

  fn set_nonblocking(fd: RawFd) {
    unsafe {
      let flags = libc::fcntl(fd, libc::F_GETFL);
      assert!(flags != -1);
      let res = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
      assert!(res != -1);
    }
  }

  extern "C" fn io_record_magic(events: u32, data: *mut u8) {
    assert_ne!(events & runtime_native::abi::RT_IO_READABLE, 0);
    record_magic(data);
  }

  let mut heap = GcHeap::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let mut fds = [0i32; 2];
  unsafe {
    assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
  }
  let read_fd = FdGuard(fds[0]);
  let write_fd = FdGuard(fds[1]);
  set_nonblocking(read_fd.0);

  let watcher = runtime_native::rt_io_register_rooted(
    read_fd.0,
    runtime_native::abi::RT_IO_READABLE,
    io_record_magic,
    obj,
  );
  assert_ne!(watcher.0, 0);

  // Move/collect while the watcher is still registered (before any readiness event).
  collect_major(&mut heap);
  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  // Trigger readability.
  unsafe {
    let byte: u8 = 1;
    let n = libc::write(write_fd.0, (&byte as *const u8).cast(), 1);
    assert_eq!(n, 1);
  }

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "I/O callback did not run in time");
    std::thread::yield_now();
  }

  // The watcher is still registered, so the object should remain alive.
  collect_major(&mut heap);
  assert!(!runtime_native::rt_weak_get(weak).is_null());

  runtime_native::rt_io_unregister(watcher);

  // `rt_io_unregister` defers running drop hooks to a microtask checkpoint so callbacks can safely
  // unregister themselves without freeing their `data` while still executing. Drain microtasks so
  // the rooted wrapper is dropped before we expect GC to reclaim the object.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll();
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted I/O watcher was unregistered (root not released?)"
    );
    std::thread::yield_now();
  }
}
