use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::sync::{Parker, Unparker};

use crate::abi::TaskId;
use crate::gc::HandleId;
use crate::sync::{GcAwareMutex, GcAwareRwLock};
use crate::threading::{self, ThreadKind};

#[path = "parallel_for.rs"]
mod parallel_for_impl;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Chunking {
  Auto,
  Fixed(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct WorkEstimate {
  pub items: usize,
  pub cost: u64,
}

pub type CostModelFn = fn(WorkEstimate) -> bool;

fn default_cost_model(work: WorkEstimate) -> bool {
  // Stub: parallelize only when the range is large enough to amortize spawn/join overhead.
  work.items >= parallel_for_impl::min_grain() && work.cost >= parallel_for_impl::min_grain() as u64
}

static COST_MODEL: GcAwareRwLock<CostModelFn> = GcAwareRwLock::new(default_cost_model);

pub fn set_cost_model(f: CostModelFn) {
  *COST_MODEL.write() = f;
}

pub fn should_parallelize(work: WorkEstimate) -> bool {
  let f = *COST_MODEL.read();
  f(work)
}

thread_local! {
  static LOCAL_WORKER: Cell<*const Worker<Arc<TaskState>>> = Cell::new(ptr::null());
  static WORKER_INDEX: Cell<usize> = Cell::new(usize::MAX);
}

fn local_worker_ptr() -> Option<(*const Worker<Arc<TaskState>>, usize)> {
  // `rt_parallel_*` entrypoints can be invoked from other thread-local destructors during TLS
  // teardown. If this TLS key has already been destroyed, `LocalKey::with` would panic with
  // `AccessError` and abort the process (`abort_on_dtor_unwind`). Treat an inaccessible TLS key as
  // "not a worker thread" and fall back to the global injector queue.
  let ptr = LOCAL_WORKER.try_with(|worker| worker.get()).ok()?;
  if ptr.is_null() {
    return None;
  }
  let idx = WORKER_INDEX.try_with(|idx| idx.get()).ok()?;
  Some((ptr, idx))
}

/// Internal parallel scheduler state.
///
/// This lives inside the global [`Runtime`](crate::Runtime) and is surfaced to
/// generated/native code through the exported `rt_parallel_*` entrypoints.
pub(crate) struct ParallelRuntime {
  scheduler: Scheduler,
  live_task_ids: GcAwareMutex<HashSet<u64>>,
}

impl ParallelRuntime {
  pub(crate) fn new() -> Self {
    Self {
      scheduler: Scheduler::new(),
      live_task_ids: GcAwareMutex::new(HashSet::new()),
    }
  }

  /// Spawn a task onto the work-stealing pool without returning a `TaskId`.
  ///
  /// This is used by async integrations that surface completion via a promise
  /// instead of `rt_parallel_join`.
  pub(crate) fn spawn_detached(&self, task: extern "C" fn(*mut u8), data: *mut u8) {
    // Ensure the caller thread participates in GC safepoints.
    threading::register_current_thread(ThreadKind::External);
    // Mirror the `spawn`/`join` entrypoints: if a stop-the-world is active, do
    // not enqueue additional work until we've observed the request.
    threading::safepoint_poll();

    let task_state = Arc::new(TaskState::new(task, data));
    self.scheduler.enqueue(task_state);
  }

  pub(crate) fn spawn(&self, task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
    // Ensure the caller thread participates in GC safepoints.
    threading::register_current_thread(ThreadKind::External);
    // If a stop-the-world is active, don't enqueue new work (and don't block on
    // scheduler locks) until we've observed the safepoint request.
    threading::safepoint_poll();

    let task_state = Arc::new(TaskState::new(task, data));
    self.scheduler.enqueue(task_state.clone());
    let id = Arc::into_raw(task_state) as u64;
    {
      let mut live = self.live_task_ids.lock();
      live.insert(id);
    }
    TaskId(id)
  }

  pub(crate) fn spawn_rooted(&self, task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
    threading::register_current_thread(ThreadKind::External);
    threading::safepoint_poll();

    let handle = crate::roots::global_persistent_handle_table().alloc(data);
    self.spawn_rooted_handle(task, handle)
  }

  pub(crate) fn spawn_rooted_handle(&self, task: extern "C" fn(*mut u8), handle: HandleId) -> TaskId {
    let task_state = Arc::new(TaskState::new_with_root(task, ptr::null_mut(), Some(handle)));

    self.scheduler.enqueue(task_state.clone());
    let id = Arc::into_raw(task_state) as u64;
    {
      let mut live = self.live_task_ids.lock();
      live.insert(id);
    }
    TaskId(id)
  }

  /// Like [`ParallelRuntime::spawn_rooted`], but takes the GC-managed `data` pointer as a `GcHandle`
  /// (pointer-to-slot).
  ///
  /// # Safety
  /// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot that contains a
  /// GC-managed object base pointer.
  pub(crate) unsafe fn spawn_rooted_h(&self, task: extern "C" fn(*mut u8), slot: crate::roots::GcHandle) -> TaskId {
    threading::register_current_thread(ThreadKind::External);
    threading::safepoint_poll();

    let handle = crate::roots::global_persistent_handle_table().alloc_from_slot(slot);
    self.spawn_rooted_handle(task, handle)
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
    let mut live = self.live_task_ids.lock();
    for &TaskId(id) in ids {
      // `TaskId` is an opaque handle. Reject clearly-invalid pointers and unknown
      // task IDs, but tolerate duplicates within the same join call by
      // de-duplicating before `Arc::from_raw` (avoids double-decrementing the
      // leaked strong count).
      if id == 0 || (id as usize) % std::mem::align_of::<TaskState>() != 0 {
        std::process::abort();
      }
      if !seen.insert(id) {
        continue;
      }
      if !live.remove(&id) {
        std::process::abort();
      }

      let ptr = id as usize as *const TaskState;
      tasks.push(unsafe { Arc::from_raw(ptr) });
    }
    drop(live);

    // Consume the task list so each `Arc<TaskState>` is dropped as soon as that
    // task completes, rather than keeping the full vector alive until the end
    // of the join call. This reduces peak per-task bookkeeping memory when
    // joining large batches.
    for task in tasks {
      while !task.done.load(Ordering::Acquire) {
        // Join can block waiting for other tasks. Poll the GC safepoint here so
        // an STW request doesn't have to wait for the join loop to make
        // progress.
        threading::safepoint_poll();

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
    self.parallel_for_with_chunking(start, end, body, data, Chunking::Auto);
  }

  pub(crate) fn parallel_for_rooted(
    &self,
    start: usize,
    end: usize,
    body: extern "C" fn(usize, *mut u8),
    data: *mut u8,
  ) {
    self.parallel_for_rooted_with_chunking(start, end, body, data, Chunking::Auto);
  }

  pub(crate) fn parallel_for_with_chunking(
    &self,
    start: usize,
    end: usize,
    body: extern "C" fn(usize, *mut u8),
    data: *mut u8,
    chunking: Chunking,
  ) {
    parallel_for_impl::parallel_for(self, start, end, body, data, chunking);
  }

  pub(crate) fn parallel_for_rooted_with_chunking(
    &self,
    start: usize,
    end: usize,
    body: extern "C" fn(usize, *mut u8),
    data: *mut u8,
    chunking: Chunking,
  ) {
    parallel_for_impl::parallel_for_rooted(self, start, end, body, data, chunking);
  }

  pub(crate) fn worker_count(&self) -> usize {
    self.scheduler.worker_count
  }
}

type TaskFn = extern "C" fn(*mut u8);

struct TaskState {
  func: TaskFn,
  data: *mut u8,
  gc_root: Mutex<Option<HandleId>>,
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
    Self::new_with_root(func, data, None)
  }

  fn new_with_root(func: TaskFn, data: *mut u8, gc_root: Option<HandleId>) -> Self {
    Self {
      func,
      data,
      gc_root: Mutex::new(gc_root),
      done: AtomicBool::new(false),
      done_lock: Mutex::new(()),
      done_cv: Condvar::new(),
    }
  }

  fn run(self: &Arc<Self>) {
    crate::rt_trace::tasks_executed_inc();

    let gc_root = self
      .gc_root
      .lock()
      .unwrap_or_else(|_| std::process::abort())
      .take();
    let data = match gc_root {
      Some(h) => crate::roots::global_persistent_handle_table()
        .get(h)
        .unwrap_or_else(|| std::process::abort()),
      None => self.data,
    };

    crate::ffi::invoke_cb1(self.func, data);
    if let Some(h) = gc_root {
      let _ = crate::roots::global_persistent_handle_table().free(h);
    }

    // Update `done` while holding the mutex to avoid lost wake-ups between the
    // joiner checking the flag and beginning to wait on the condvar.
    // If we contend on the completion mutex, enter a GC-safe region while waiting so stop-the-world
    // coordination doesn't deadlock on a thread blocked in `Mutex::lock`.
    //
    // Keep the GC-safe guard alive until after the mutex is released: dropping `GcSafeGuard` may
    // block while a stop-the-world is active.
    let mut gc_safe: Option<threading::GcSafeGuard> = None;
    {
      let _guard = match self.done_lock.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::WouldBlock) => {
          gc_safe = Some(threading::enter_gc_safe_region());
          self
            .done_lock
            .lock()
            .unwrap_or_else(|_| std::process::abort())
        }
        Err(std::sync::TryLockError::Poisoned(_)) => std::process::abort(),
      };
      self.done.store(true, Ordering::Release);
      self.done_cv.notify_all();
    }
    drop(gc_safe);
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
    // Prefer the namespaced env var but accept `RT_NUM_THREADS` as a legacy alias.
    let worker_count = std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS")
      .ok()
      .or_else(|| std::env::var("RT_NUM_THREADS").ok())
      .and_then(|v| v.parse::<usize>().ok())
      .filter(|&n| n > 0)
      .unwrap_or_else(|| {
        let default = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        if cfg!(debug_assertions) {
          default.min(32)
        } else {
          default
        }
      });

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
    enum Outcome {
      Task(Arc<TaskState>),
      Retry,
      Empty,
    }

    let try_once = || {
      if let Some((ptr, idx)) = local_worker_ptr() {
        let local = unsafe { &*ptr };
        if let Some(task) = local.pop() {
          return Outcome::Task(task);
        }

        match self.inner.injector.steal_batch_and_pop(local) {
          Steal::Success(task) => return Outcome::Task(task),
          Steal::Retry => return Outcome::Retry,
          Steal::Empty => {}
        }

        let n = self.inner.stealers.len();
        if n > 1 {
          for offset in 1..n {
            let victim = (idx + offset) % n;
            crate::rt_trace::steals_attempted_inc();
            match self.inner.stealers[victim].steal_batch_and_pop(local) {
              Steal::Success(task) => {
                crate::rt_trace::steals_succeeded_inc();
                return Outcome::Task(task);
              }
              Steal::Retry => return Outcome::Retry,
              Steal::Empty => {
                // `steal_batch_and_pop` can fail to make progress when the victim only has a single
                // task available. Fall back to stealing a single task to keep the pool
                // work-conserving, which also avoids "no overlap" behavior in small-task cases.
                match self.inner.stealers[victim].steal() {
                  Steal::Success(task) => {
                    crate::rt_trace::steals_succeeded_inc();
                    return Outcome::Task(task);
                  }
                  Steal::Retry => return Outcome::Retry,
                  Steal::Empty => {}
                }
              }
            }
          }
        }

        Outcome::Empty
      } else {
        match self.inner.injector.steal() {
          Steal::Success(task) => return Outcome::Task(task),
          Steal::Retry => return Outcome::Retry,
          Steal::Empty => {}
        }

        for stealer in &self.inner.stealers {
          crate::rt_trace::steals_attempted_inc();
          match stealer.steal() {
            Steal::Success(task) => {
              crate::rt_trace::steals_succeeded_inc();
              return Outcome::Task(task);
            }
            Steal::Retry => return Outcome::Retry,
            Steal::Empty => {}
          }
        }

        Outcome::Empty
      }
    };

    // `Steal::Retry` indicates a concurrent steal in progress. Treat it as a
    // transient state and retry a few times before declaring the pool empty; this
    // avoids joiner threads parking while work is available but temporarily
    // contended.
    for _ in 0..4 {
      match try_once() {
        Outcome::Task(task) => return Some(task),
        Outcome::Retry => continue,
        Outcome::Empty => return None,
      }
    }

    None
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
    // Poll the GC safepoint barrier on every loop iteration so stop-the-world
    // requests are observed promptly even while the worker is idle/spinning or
    // retrying steals.
    threading::safepoint_poll();

    if let Some(task) = local.pop() {
      spins = 0;
      // Before running mutator code, poll the GC safepoint.
      threading::safepoint_poll();
      task.run();
      continue;
    }

    match inner.injector.steal_batch_and_pop(&local) {
      Steal::Success(task) => {
        spins = 0;
        threading::safepoint_poll();
        task.run();
        continue;
      }
      Steal::Retry => {
        // `Steal::Retry` indicates transient contention on the injector. Ensure we still cooperate
        // with stop-the-world requests while spinning in this retry loop.
        threading::safepoint_poll();
        continue;
      }
      Steal::Empty => {}
    }

    let n = inner.stealers.len();
    if n > 1 {
      for offset in 1..n {
        let victim = (idx + offset) % n;
        crate::rt_trace::steals_attempted_inc();
        match inner.stealers[victim].steal_batch_and_pop(&local) {
          Steal::Success(task) => {
            crate::rt_trace::steals_succeeded_inc();
            spins = 0;
            threading::safepoint_poll();
            task.run();
            continue 'work;
          }
          Steal::Retry => {
            // Like the injector path above, a retry indicates contention. Poll the safepoint so a
            // stop-the-world request doesn't time out waiting for an idle worker spinning here.
            threading::safepoint_poll();
            continue 'work;
          }
          Steal::Empty => {
            // See comment in `Scheduler::try_pop`: `steal_batch_and_pop` may not steal from a
            // victim with only one queued task. Fall back to stealing a single task to improve
            // fairness and to allow small task sets to actually execute concurrently.
            match inner.stealers[victim].steal() {
              Steal::Success(task) => {
                crate::rt_trace::steals_succeeded_inc();
                spins = 0;
                threading::safepoint_poll();
                task.run();
                continue 'work;
              }
              Steal::Retry => {
                threading::safepoint_poll();
                continue 'work;
              }
              Steal::Empty => {}
            }
          }
        }
      }
    }

    // No work available. Poll the safepoint barrier before entering the short spin/park loop so
    // stop-the-world GC can reliably stop even a large pool of idle workers.
    threading::safepoint_poll();

    if spins < 10 {
      spins += 1;
      std::hint::spin_loop();
      continue;
    }
    spins = 0;

    let parked = threading::ParkedGuard::new();
    crate::rt_trace::worker_park_inc();
    parker.park_timeout(Duration::from_millis(1));
    crate::rt_trace::worker_unpark_inc();
    drop(parked);

    // Before running mutator code, poll the GC safepoint.
    threading::safepoint_poll();
  }
}

#[repr(C)]
struct ClosureTask {
  f: Box<dyn FnOnce() + Send + 'static>,
}

extern "C" fn run_closure_task(data: *mut u8) {
  let task = unsafe { Box::from_raw(data as *mut ClosureTask) };
  (task.f)();
}

/// Spawn a Rust closure as a runtime-native task.
///
/// This is a Rust-only convenience API; compiler-generated code should use the
/// exported `rt_parallel_spawn` symbol.
pub fn spawn<F>(f: F) -> TaskId
where
  F: FnOnce() + Send + 'static,
{
  let task = Box::new(ClosureTask { f: Box::new(f) });
  crate::rt_parallel_spawn(run_closure_task, Box::into_raw(task) as *mut u8)
}

/// Join a slice of task ids spawned by `rt_parallel_spawn` / [`spawn`].
pub fn join(tasks: &[TaskId]) {
  crate::rt_parallel_join(tasks.as_ptr(), tasks.len());
}

#[repr(C)]
struct ParForChunk {
  start: usize,
  end: usize,
  body: Arc<dyn Fn(usize) + Send + Sync + 'static>,
}

extern "C" fn par_for_chunk_task(data: *mut u8) {
  let chunk = unsafe { Box::from_raw(data as *mut ParForChunk) };
  for i in chunk.start..chunk.end {
    // `parallel_for` owns the iteration loop in the runtime. Poll the GC
    // safepoint here so stop-the-world requests don't have to rely on the user
    // callback to hit a safepoint.
    threading::safepoint_poll();
    (chunk.body)(i);
  }
}

/// Parallelize a simple for-loop over `range`.
///
/// This is intended for runtime-native internal use and for future compiler
/// codegen helpers (e.g. array map/filter fusion) where the compiler can
/// guarantee no shared mutable state between iterations.
pub fn parallel_for<F>(range: Range<usize>, body: F, chunking: Chunking)
where
  F: Fn(usize) + Send + Sync + 'static,
{
  threading::register_current_thread(ThreadKind::External);

  if range.end <= range.start {
    return;
  }

  let len = range.end - range.start;
  let workers = crate::rt_parallel().worker_count();

  let estimate = WorkEstimate {
    items: len,
    cost: len as u64,
  };
  if workers <= 1 || !should_parallelize(estimate) {
    for i in range {
      threading::safepoint_poll();
      body(i);
    }
    return;
  }

  let min_grain = parallel_for_impl::min_grain();
  let chunk_size = match chunking {
    Chunking::Fixed(size) => size.max(1),
    Chunking::Auto => {
      let target_chunks = workers.saturating_mul(4).max(1);
      (len.div_ceil(target_chunks)).max(min_grain)
    }
  };

  if chunk_size >= len {
    for i in range {
      threading::safepoint_poll();
      body(i);
    }
    return;
  }

  let body: Arc<dyn Fn(usize) + Send + Sync + 'static> = Arc::new(body);
  let mut tasks: Vec<TaskId> = Vec::new();

  let mut chunk_start = range.start;
  while chunk_start < range.end {
    let chunk_end = range.end.min(chunk_start.saturating_add(chunk_size));
    let chunk = Box::new(ParForChunk {
      start: chunk_start,
      end: chunk_end,
      body: body.clone(),
    });
    tasks.push(crate::rt_parallel_spawn(
      par_for_chunk_task,
      Box::into_raw(chunk) as *mut u8,
    ));
    chunk_start = chunk_end;
  }

  join(&tasks);
}

/// Like [`ParallelRuntime::parallel_for`] / `rt_parallel_for`, but allows an
/// explicit chunking strategy.
pub fn parallel_for_raw(
  range: Range<usize>,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
  chunking: Chunking,
) {
  crate::rt_parallel().parallel_for_with_chunking(range.start, range.end, body, data, chunking);
}
