use std::collections::{HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::abi::TaskId;
use crate::threading::{self, ThreadKind};

mod parallel_for;

/// Internal parallel scheduler state.
///
/// This lives inside the global [`Runtime`](crate::Runtime) and is surfaced to
/// generated/native code through the exported `rt_parallel_*` entrypoints.
pub(crate) struct ParallelRuntime {
  scheduler: Scheduler,
}

impl ParallelRuntime {
  pub(crate) fn new() -> Self {
    Self {
      scheduler: Scheduler::new(),
    }
  }

  pub(crate) fn spawn(&self, task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
    // Ensure the caller thread participates in GC safepoints.
    threading::register_current_thread(ThreadKind::External);

    let task_state = Arc::new(TaskState::new(task, data));
    self.scheduler.enqueue(task_state.clone());
    TaskId(Arc::into_raw(task_state) as u64)
  }

  pub(crate) fn join(&self, tasks: *const TaskId, count: usize) {
    threading::register_current_thread(ThreadKind::External);

    if count == 0 {
      return;
    }
    if tasks.is_null() {
      std::process::abort();
    }

    let ids = unsafe { std::slice::from_raw_parts(tasks, count) };
    let mut seen: HashSet<u64> = HashSet::with_capacity(ids.len());
    let mut tasks: Vec<Arc<TaskState>> = Vec::with_capacity(ids.len());
    for &TaskId(id) in ids {
      // `TaskId` is an opaque handle and must be unique within a join call.
      // Duplicates would cause UB (double `Arc::from_raw`), so fail loudly.
      if id == 0
        || (id as usize) % std::mem::align_of::<TaskState>() != 0
        || !seen.insert(id)
      {
        std::process::abort();
      }

      let ptr = id as usize as *const TaskState;
      tasks.push(unsafe { Arc::from_raw(ptr) });
    }

    for task in &tasks {
      while !task.done.load(Ordering::Acquire) {
        if let Some(other) = self.scheduler.try_pop() {
          other.run();
          continue;
        }

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

    self.done.store(true, Ordering::Release);
    self.done_cv.notify_all();
  }
}

struct SchedulerInner {
  queue: Mutex<VecDeque<Arc<TaskState>>>,
  queue_cv: Condvar,
}

struct Scheduler {
  worker_count: usize,
  inner: Arc<SchedulerInner>,
}

impl Scheduler {
  fn new() -> Self {
    let worker_count = thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1);

    let inner = Arc::new(SchedulerInner {
      queue: Mutex::new(VecDeque::new()),
      queue_cv: Condvar::new(),
    });

    for idx in 0..worker_count {
      let inner = inner.clone();
      if thread::Builder::new()
        .name(format!("rt-par-worker-{idx}"))
        .spawn(move || worker_loop(inner))
        .is_err()
      {
        // Never unwind across our FFI boundary (this can be reached during
        // `rt_ensure_init` when initializing the runtime).
        std::process::abort();
      }
    }

    Self {
      worker_count,
      inner,
    }
  }

  fn enqueue(&self, task: Arc<TaskState>) {
    let mut q = self.inner.queue.lock().unwrap_or_else(|_| std::process::abort());
    q.push_back(task);
    self.inner.queue_cv.notify_one();
  }

  fn try_pop(&self) -> Option<Arc<TaskState>> {
    let mut q = self.inner.queue.lock().unwrap_or_else(|_| std::process::abort());
    q.pop_front()
  }
}

fn worker_loop(inner: Arc<SchedulerInner>) -> ! {
  threading::register_current_thread(ThreadKind::Worker);

  loop {
    let task = {
      let mut q = inner.queue.lock().unwrap_or_else(|_| std::process::abort());
      loop {
        if let Some(task) = q.pop_front() {
          break task;
        }

        threading::set_parked(true);
        q = inner
          .queue_cv
          .wait(q)
          .unwrap_or_else(|_| std::process::abort());
        threading::set_parked(false);
        // Before running mutator code, poll the GC safepoint.
        threading::safepoint_poll();
      }
    };

    task.run();
  }
}
