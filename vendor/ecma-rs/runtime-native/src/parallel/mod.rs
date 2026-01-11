use std::cell::Cell;
use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::sync::{Parker, Unparker};

use crate::abi::TaskId;
use crate::threading::{self, ThreadKind};

mod parallel_for;

thread_local! {
  static LOCAL_WORKER: Cell<*const Worker<Arc<TaskState>>> = Cell::new(ptr::null());
  static WORKER_INDEX: Cell<usize> = Cell::new(usize::MAX);
}

fn local_worker_ptr() -> Option<(*const Worker<Arc<TaskState>>, usize)> {
  LOCAL_WORKER.with(|worker| {
    let ptr = worker.get();
    if ptr.is_null() {
      None
    } else {
      let idx = WORKER_INDEX.with(|idx| idx.get());
      Some((ptr, idx))
    }
  })
}

/// Internal parallel scheduler state.
///
/// This lives inside the global [`Runtime`](crate::Runtime) and is surfaced to
/// generated/native code through the exported `rt_parallel_*` entrypoints.
pub(crate) struct ParallelRuntime {
  scheduler: Scheduler,
  live_task_ids: Mutex<HashSet<u64>>,
}

impl ParallelRuntime {
  pub(crate) fn new() -> Self {
    Self {
      scheduler: Scheduler::new(),
      live_task_ids: Mutex::new(HashSet::new()),
    }
  }

  pub(crate) fn spawn(&self, task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
    // Ensure the caller thread participates in GC safepoints.
    threading::register_current_thread(ThreadKind::External);

    let task_state = Arc::new(TaskState::new(task, data));
    self.scheduler.enqueue(task_state.clone());
    let id = Arc::into_raw(task_state) as u64;
    {
      let mut live = self
        .live_task_ids
        .lock()
        .unwrap_or_else(|_| std::process::abort());
      live.insert(id);
    }
    TaskId(id)
  }

  pub(crate) fn join(&self, tasks: *const TaskId, count: usize) {
    threading::register_current_thread(ThreadKind::External);
    // If a stop-the-world is active, do not proceed into blocking waits or run
    // additional mutator work until we've observed the safepoint request.
    threading::safepoint_poll();

    if count == 0 {
      return;
    }
    if tasks.is_null() {
      std::process::abort();
    }

    // `slice::from_raw_parts` requires alignment. Reject misaligned pointers
    // before constructing the slice to avoid UB in case of ABI misuse.
    if (tasks as usize) % std::mem::align_of::<TaskId>() != 0 {
      std::process::abort();
    }

    let ids = unsafe { std::slice::from_raw_parts(tasks, count) };
    let mut seen: HashSet<u64> = HashSet::with_capacity(ids.len());
    let mut tasks: Vec<Arc<TaskState>> = Vec::with_capacity(ids.len());
    let mut live = self
      .live_task_ids
      .lock()
      .unwrap_or_else(|_| std::process::abort());
    for &TaskId(id) in ids {
      // `TaskId` is an opaque handle and must be unique within a join call.
      // Duplicates would cause UB (double `Arc::from_raw`), so fail loudly.
      if id == 0
        || (id as usize) % std::mem::align_of::<TaskState>() != 0
        || !seen.insert(id)
        || !live.remove(&id)
      {
        std::process::abort();
      }

      let ptr = id as usize as *const TaskState;
      tasks.push(unsafe { Arc::from_raw(ptr) });
    }
    drop(live);

    for task in &tasks {
      while !task.done.load(Ordering::Acquire) {
        if let Some(other) = self.scheduler.try_pop() {
          // Before running mutator code, poll the GC safepoint (matches the
          // worker thread loop behavior).
          threading::safepoint_poll();
          other.run();
          continue;
        }

        // Avoid deadlocking stop-the-world GC: treat this thread as GC-safe while blocked on the
        // task completion condvar.
        //
        // Note: acquire `GcSafeGuard` *before* locking `done_lock` so lock acquisition itself can't
        // deadlock the GC coordinator if contended.
        let gc_safe = threading::enter_gc_safe_region();
        {
          let mut guard = task
            .done_lock
            .lock()
            .unwrap_or_else(|_| std::process::abort());
          while !task.done.load(Ordering::Acquire) {
            guard = task
              .done_cv
              .wait(guard)
              .unwrap_or_else(|_| std::process::abort());
          }
        }
        drop(gc_safe);
      }
    }
  }

  pub(crate) fn parallel_for(
    &self,
    start: usize,
    end: usize,
    body: extern "C" fn(usize, *mut u8),
    data: *mut u8,
  ) {
    parallel_for::parallel_for(self, start, end, body, data);
  }

  fn worker_count(&self) -> usize {
    self.scheduler.worker_count
  }
}

type TaskFn = extern "C" fn(*mut u8);

struct TaskState {
  func: TaskFn,
  data: *mut u8,
  done: AtomicBool,
  done_lock: Mutex<()>,
  done_cv: Condvar,
}

// Tasks are constructed from raw pointers coming from generated code / FFI.
// The runtime assumes the caller upholds the safety contract that `data` is
// valid for the duration of the task, and that any sharing across threads is
// data-race-free (compiler-enforced for generated code).
unsafe impl Send for TaskState {}
unsafe impl Sync for TaskState {}

impl TaskState {
  fn new(func: TaskFn, data: *mut u8) -> Self {
    Self {
      func,
      data,
      done: AtomicBool::new(false),
      done_lock: Mutex::new(()),
      done_cv: Condvar::new(),
    }
  }

  fn run(self: &Arc<Self>) {
    let res = catch_unwind(AssertUnwindSafe(|| (self.func)(self.data)));
    if res.is_err() {
      // Never unwind across our `extern "C"` boundary.
      std::process::abort();
    }

    // Update `done` while holding the mutex to avoid lost wake-ups between the
    // joiner checking the flag and beginning to wait on the condvar.
    let _guard = self
      .done_lock
      .lock()
      .unwrap_or_else(|_| std::process::abort());
    self.done.store(true, Ordering::Release);
    self.done_cv.notify_all();
  }
}

struct SchedulerInner {
  injector: Injector<Arc<TaskState>>,
  stealers: Vec<Stealer<Arc<TaskState>>>,
  unparkers: Vec<Unparker>,
  next_unparker: AtomicUsize,
}

struct Scheduler {
  worker_count: usize,
  inner: Arc<SchedulerInner>,
}

impl Scheduler {
  fn new() -> Self {
    let worker_count = std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS")
      .ok()
      .and_then(|v| v.parse::<usize>().ok())
      .filter(|&n| n > 0)
      .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

    let mut workers: Vec<Worker<Arc<TaskState>>> = Vec::with_capacity(worker_count);
    let mut stealers: Vec<Stealer<Arc<TaskState>>> = Vec::with_capacity(worker_count);
    let mut parkers: Vec<Parker> = Vec::with_capacity(worker_count);
    let mut unparkers: Vec<Unparker> = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
      let worker = Worker::new_lifo();
      stealers.push(worker.stealer());
      workers.push(worker);

      let parker = Parker::new();
      unparkers.push(parker.unparker().clone());
      parkers.push(parker);
    }

    let inner = Arc::new(SchedulerInner {
      injector: Injector::new(),
      stealers,
      unparkers,
      next_unparker: AtomicUsize::new(0),
    });

    for (idx, (local, parker)) in workers.into_iter().zip(parkers).enumerate() {
      let inner = inner.clone();
      if thread::Builder::new()
        .name(format!("rt-par-worker-{idx}"))
        .spawn(move || worker_loop(idx, inner, local, parker))
        .is_err()
      {
        // Never unwind across our FFI boundary (this can be reached during
        // `rt_ensure_init` when initializing the runtime).
        std::process::abort();
      }
    }

    Self { worker_count, inner }
  }

  fn wake_one(&self) {
    let n = self.inner.unparkers.len();
    if n == 0 {
      return;
    }
    let idx = self.inner.next_unparker.fetch_add(1, Ordering::Relaxed) % n;
    self.inner.unparkers[idx].unpark();
  }

  fn enqueue(&self, task: Arc<TaskState>) {
    if let Some((ptr, _idx)) = local_worker_ptr() {
      unsafe { (&*ptr).push(task) };
    } else {
      self.inner.injector.push(task);
    }
    self.wake_one();
  }

  fn try_pop(&self) -> Option<Arc<TaskState>> {
    if let Some((ptr, idx)) = local_worker_ptr() {
      let local = unsafe { &*ptr };
      if let Some(task) = local.pop() {
        return Some(task);
      }

      match self.inner.injector.steal_batch_and_pop(local) {
        Steal::Success(task) => return Some(task),
        Steal::Retry => return None,
        Steal::Empty => {}
      }

      let n = self.inner.stealers.len();
      if n > 1 {
        for offset in 1..n {
          let victim = (idx + offset) % n;
          match self.inner.stealers[victim].steal_batch_and_pop(local) {
            Steal::Success(task) => return Some(task),
            Steal::Retry => return None,
            Steal::Empty => {}
          }
        }
      }

      None
    } else {
      match self.inner.injector.steal() {
        Steal::Success(task) => return Some(task),
        Steal::Retry => return None,
        Steal::Empty => {}
      }

      for stealer in &self.inner.stealers {
        match stealer.steal() {
          Steal::Success(task) => return Some(task),
          Steal::Retry => return None,
          Steal::Empty => {}
        }
      }

      None
    }
  }
}

fn worker_loop(
  idx: usize,
  inner: Arc<SchedulerInner>,
  local: Worker<Arc<TaskState>>,
  parker: Parker,
) -> ! {
  threading::register_current_thread(ThreadKind::Worker);

  LOCAL_WORKER.with(|worker| worker.set(&local));
  WORKER_INDEX.with(|index| index.set(idx));

  let mut spins = 0usize;
  'work: loop {
    if let Some(task) = local.pop() {
      spins = 0;
      task.run();
      continue;
    }

    match inner.injector.steal_batch_and_pop(&local) {
      Steal::Success(task) => {
        spins = 0;
        task.run();
        continue;
      }
      Steal::Retry => continue,
      Steal::Empty => {}
    }

    let n = inner.stealers.len();
    if n > 1 {
      for offset in 1..n {
        let victim = (idx + offset) % n;
        match inner.stealers[victim].steal_batch_and_pop(&local) {
          Steal::Success(task) => {
            spins = 0;
            task.run();
            continue 'work;
          }
          Steal::Retry => continue 'work,
          Steal::Empty => {}
        }
      }
    }

    if spins < 10 {
      spins += 1;
      std::hint::spin_loop();
      continue;
    }
    spins = 0;

    threading::set_parked(true);
    parker.park_timeout(Duration::from_millis(1));
    threading::set_parked(false);

    // Before running mutator code, poll the GC safepoint.
    threading::safepoint_poll();
  }
}
