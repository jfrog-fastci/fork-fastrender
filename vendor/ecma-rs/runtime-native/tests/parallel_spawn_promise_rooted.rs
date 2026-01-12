use runtime_native::abi::PromiseRef;
use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;
use runtime_native::PromiseLayout;
use runtime_native::TypeDescriptor;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

const MAGIC: u64 = 0x0123_4567_89AB_CDEF;
const TIMEOUT: Duration = Duration::from_secs(if cfg!(debug_assertions) { 30 } else { 10 });

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

static ROOTED_H_PROMISE_STARTED: AtomicBool = AtomicBool::new(false);
static ROOTED_H_PROMISE_RELEASE: AtomicBool = AtomicBool::new(false);
static ROOTED_H_PROMISE_PTR: AtomicUsize = AtomicUsize::new(0);

extern "C" fn rooted_h_record_ptr_and_block(data: *mut u8, promise: PromiseRef) {
  ROOTED_H_PROMISE_PTR.store(data as usize, Ordering::SeqCst);
  ROOTED_H_PROMISE_STARTED.store(true, Ordering::SeqCst);
  while !ROOTED_H_PROMISE_RELEASE.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  unsafe {
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

  let mut tasks: Vec<runtime_native::abi::TaskId> = Vec::with_capacity(workers);
  for _ in 0..workers {
    tasks.push(runtime_native::rt_parallel_spawn(
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

  let deadline = Instant::now() + TIMEOUT;
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
  let deadline = Instant::now() + TIMEOUT;
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
  let deadline = Instant::now() + TIMEOUT;
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

#[test]
fn parallel_spawn_promise_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the current thread claims the event-loop identity so the worker thread below registers
  // as `External` rather than becoming the event loop.
  let _ = runtime_native::rt_async_poll();

  ROOTED_H_PROMISE_STARTED.store(false, Ordering::SeqCst);
  ROOTED_H_PROMISE_PTR.store(0, Ordering::SeqCst);
  ROOTED_H_PROMISE_RELEASE.store(false, Ordering::Release);

  struct ReleaseOnDrop;
  impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
      ROOTED_H_PROMISE_RELEASE.store(true, Ordering::Release);
    }
  }
  let _release_on_drop = ReleaseOnDrop;

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Pointers are treated as opaque addresses; they do not need to be dereferenceable in this test.
  let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot_value) as usize;

  let promise = std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to spawn rooted-h work while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<PromiseRef>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` is a valid slot pointer.
      let promise = unsafe {
        runtime_native::rt_parallel_spawn_promise_rooted_h(
          rooted_h_record_ptr_and_block,
          slot_ptr,
          PromiseLayout::of::<()>(),
        )
      };
      c_done_tx.send(promise).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's spawn attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
        if t.id() == c_id {
          native_safe = t.is_native_safe();
        }
      });

      if native_safe {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("thread C did not enter a GC-safe region while blocked on the persistent handle table lock");
      }
      std::thread::yield_now();
    }

    // Update the slot while thread C is blocked. If `rt_parallel_spawn_promise_rooted_h` (or its
    // internal plumbing) incorrectly reads the slot before acquiring the lock, it would still
    // observe the old value.
    slot_value = new_value;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("spawn should complete after lock is released")
  });

  // Wait for the worker task to start so we know it has observed the rooted pointer.
  let deadline = Instant::now() + TIMEOUT;
  while !ROOTED_H_PROMISE_STARTED.load(Ordering::SeqCst) {
    assert!(
      Instant::now() < deadline,
      "timeout waiting for rooted-h parallel_spawn_promise task to start"
    );
    std::thread::yield_now();
  }

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 2,
    "rooted-h parallel_spawn_promise should allocate one persistent handle for `data` and one for the promise while task is pending"
  );

  assert_eq!(
    ROOTED_H_PROMISE_PTR.load(Ordering::SeqCst),
    new_value as usize,
    "parallel_spawn_promise rooted_h task must observe the slot value read after lock acquisition"
  );

  // Release the blocking task so it can settle the promise and free its persistent handle.
  ROOTED_H_PROMISE_RELEASE.store(true, Ordering::Release);

  let promise_header = promise.0.cast::<runtime_native::async_abi::PromiseHeader>();
  assert!(!promise_header.is_null());
  let start = Instant::now();
  loop {
    let state = unsafe { &*promise_header }.state.load(Ordering::Acquire);
    if state == runtime_native::async_abi::PromiseHeader::FULFILLED {
      break;
    }
    if state == runtime_native::async_abi::PromiseHeader::REJECTED {
      panic!("expected rooted-h parallel_spawn_promise task to fulfill, but it rejected");
    }
    assert!(
      start.elapsed() < Duration::from_secs(5),
      "timeout waiting for rooted-h parallel_spawn_promise promise to settle"
    );
    runtime_native::rt_async_poll();
    std::thread::yield_now();
  }

  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::roots::global_persistent_handle_table().live_count() != base_roots {
    assert!(
      Instant::now() < deadline,
      "rooted-h parallel_spawn_promise should release its persistent handle after the task completes"
    );
    std::thread::yield_now();
  }
}
