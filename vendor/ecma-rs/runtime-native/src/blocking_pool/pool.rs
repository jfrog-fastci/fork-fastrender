use crate::async_rt;
use crate::abi::PromiseRef;
use crate::sync::GcAwareMutex;
use crate::threading;
use crate::threading::ThreadKind;
use once_cell::sync::OnceCell;
use parking_lot::Condvar;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

struct WorkItem {
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  // `rt_spawn_blocking` currently returns a **legacy** `async_rt::promise::RtPromise` allocated on
  // the Rust heap (not GC-managed and therefore not relocatable). Storing the raw pointer across
  // the blocking queue is therefore safe today.
  //
  // If `rt_spawn_blocking` is ever changed to return a GC-managed promise, this field must store a
  // GC-stable handle/root (e.g. a persistent handle) instead of a raw pointer: blocking worker
  // threads are plain Rust frames and are not covered by LLVM stackmaps.
  promise: PromiseRef,
}

// Raw pointers are not `Send` by default; in the runtime ABI the caller is responsible for
// ensuring `data` is valid to access from the blocking thread pool.
unsafe impl Send for WorkItem {}

struct Shared {
  queue: GcAwareMutex<VecDeque<WorkItem>>,
  cv: Condvar,
  shutdown: AtomicBool,
}

pub(crate) struct BlockingPool {
  shared: Arc<Shared>,
  _threads: Vec<std::thread::JoinHandle<()>>,
}

static POOL: OnceCell<BlockingPool> = OnceCell::new();

pub(crate) fn global() -> &'static BlockingPool {
  POOL.get_or_init(BlockingPool::new)
}

#[doc(hidden)]
pub(super) fn debug_hold_queue_lock() -> impl Drop {
  struct Hold {
    _guard: parking_lot::MutexGuard<'static, VecDeque<WorkItem>>,
  }

  impl Drop for Hold {
    fn drop(&mut self) {}
  }

  Hold {
    _guard: global().shared.queue.lock(),
  }
}

impl BlockingPool {
  fn new() -> Self {
    let default_threads = std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1)
      // Keep the default small: the blocking pool is intended for I/O-style tasks and is lazily
      // initialized. Spawning dozens of threads on first use adds noticeable latency and can cause
      // tests/embedders to miss "wake promptly" invariants (e.g. `c_link_smoke`).
      .min(4);

    // Prefer the namespaced env var (matches `ECMA_RS_RUNTIME_NATIVE_THREADS` used by the
    // parallel scheduler) but keep `RT_BLOCKING_THREADS` as a backwards-compatible alias.
    let threads = std::env::var("ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS")
      .ok()
      .or_else(|| std::env::var("RT_BLOCKING_THREADS").ok())
      .and_then(|val| val.parse::<usize>().ok())
      .filter(|n| *n > 0)
      .unwrap_or(default_threads);

    let shared = Arc::new(Shared {
      queue: GcAwareMutex::new(VecDeque::new()),
      cv: Condvar::new(),
      shutdown: AtomicBool::new(false),
    });

    let mut handles = Vec::with_capacity(threads);
    for idx in 0..threads {
      let shared = Arc::clone(&shared);
      let builder = std::thread::Builder::new().name(format!("rt-blocking-{idx}"));
      let handle = builder
        .spawn(move || worker_loop(shared))
        .expect("failed to spawn blocking worker thread");
      handles.push(handle);
    }

    Self {
      shared,
      _threads: handles,
    }
  }

  pub(crate) fn spawn(&self, task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8) -> PromiseRef {
    // Ensure the async runtime is initialized so promise settlement can wake a thread blocked in the
    // platform reactor wait syscall (`epoll_wait`/`kevent`).
    let _ = async_rt::global();
    let promise = async_rt::promise::promise_new();

    {
      let mut q = self.shared.queue.lock();
      q.push_back(WorkItem {
        task,
        data,
        promise,
      });
    }
    self.shared.cv.notify_one();
    promise
  }
}

fn worker_loop(shared: Arc<Shared>) {
  threading::register_current_thread(ThreadKind::Io);

  loop {
    threading::safepoint_poll();

    let work = {
      let mut q = shared.queue.lock();
      loop {
        if let Some(work) = q.pop_front() {
          break Some(work);
        }
        if shared.shutdown.load(Ordering::Acquire) {
          break None;
        }
        // While idle, mark as parked so stop-the-world GC treats this thread as quiescent.
        let parked = threading::ParkedGuard::new();
        shared.cv.wait(&mut q);
        drop(parked);
      }
    };

    let Some(work) = work else {
      break;
    };

    // Before running mutator code, poll the GC safepoint.
    threading::safepoint_poll();

    // Blocking tasks execute in a GC-safe region so stop-the-world GC doesn't deadlock on a worker
    // thread blocked in a syscall or long wait.
    //
    // Contract: the task must not touch or mutate the GC heap while running (no GC allocations, no
    // dereferencing GC pointers, no write barriers).
    let gc_safe = threading::enter_gc_safe_region();

    // The task is responsible for settling the promise. If it panics we abort the process
    // deterministically instead of unwinding into the runtime.
    crate::ffi::invoke_cb2_promise(work.task, work.data, work.promise);

    drop(gc_safe);
  }
}
