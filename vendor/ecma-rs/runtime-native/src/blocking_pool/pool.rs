use crate::abi::{LegacyPromiseRef, PromiseRef};
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING};
use crate::async_rt;
use crate::async_runtime::PromiseLayout;
use crate::gc::HandleId;
use crate::sync::GcAwareMutex;
use crate::threading;
use crate::threading::ThreadKind;
use crate::trap;
use once_cell::sync::OnceCell;
use parking_lot::Condvar;
use std::alloc::Layout;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

enum WorkData {
  Unrooted(*mut u8),
  Rooted(async_rt::gc::Root),
}

enum WorkKind {
  /// Legacy `rt_spawn_blocking` task: callback receives a legacy `RtPromise` handle and settles it
  /// directly (not GC-managed).
  Legacy {
    task: extern "C" fn(*mut u8, LegacyPromiseRef),
    promise: LegacyPromiseRef,
  },

  /// GC-managed payload promise task: callback writes into an out-of-line payload buffer and
  /// returns a status tag; a microtask hop settles the promise on the event-loop thread.
  Promise {
    task: extern "C" fn(*mut u8, *mut u8) -> u8,
    promise: HandleId,
    tmp_payload: *mut u8,
    layout: PromiseLayout,
  },
}

struct WorkItem {
  data: WorkData,
  kind: WorkKind,
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

#[inline]
fn maybe_clear_external_pending(promise: *mut PromiseHeader) {
  if promise.is_null() {
    return;
  }
  if (promise as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  let prev = unsafe { &(*promise).flags }.fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel);
  if (prev & PROMISE_FLAG_EXTERNAL_PENDING) != 0 {
    async_rt::external_pending_dec();
  }
}

#[repr(C)]
struct BlockingPromiseMicrotask {
  promise: HandleId,
  tag: u8,
  tmp_payload: *mut u8,
  layout: PromiseLayout,
  ran: bool,
}

extern "C" fn run_blocking_promise_microtask(data: *mut u8) {
  if data.is_null() {
    return;
  }
  // Safety: allocated by `Box::into_raw` when enqueuing the microtask and freed by
  // `drop_blocking_promise_microtask`.
  let task = unsafe { &mut *(data as *mut BlockingPromiseMicrotask) };
  task.ran = true;

  let promise_ptr = crate::roots::global_persistent_handle_table()
    .get(task.promise)
    .unwrap_or_else(|| std::process::abort());
  let promise = PromiseRef(promise_ptr.cast());

  // Copy the result bytes from the temporary out-of-GC buffer into the promise's payload buffer.
  let dst = async_rt::promise::promise_payload_ptr(promise);
  if task.layout.size != 0 {
    if dst.is_null() || task.tmp_payload.is_null() {
      std::process::abort();
    }
    unsafe {
      std::ptr::copy_nonoverlapping(task.tmp_payload, dst, task.layout.size);
    }
  }

  unsafe {
    if task.tag == 0 {
      crate::rt_promise_fulfill(promise);
    } else {
      crate::rt_promise_reject(promise);
    }
  }

  // Free the temporary payload buffer.
  if task.layout.size != 0 && !task.tmp_payload.is_null() {
    let align = task.layout.align.max(1);
    if !align.is_power_of_two() {
      std::process::abort();
    }
    let layout = Layout::from_size_align(task.layout.size, align).unwrap_or_else(|_| std::process::abort());
    unsafe {
      std::alloc::dealloc(task.tmp_payload, layout);
    }
  }
}

extern "C" fn drop_blocking_promise_microtask(data: *mut u8) {
  if data.is_null() {
    return;
  }

  // Safety: allocated by `Box::into_raw` when enqueuing the microtask.
  let task = unsafe { Box::from_raw(data as *mut BlockingPromiseMicrotask) };

  if !task.ran {
    // If the microtask was discarded (e.g. `rt_async_cancel_all`), the promise never settles. Clear
    // the external-pending flag and decrement the global count so the runtime can report idle.
    if let Some(promise_ptr) = crate::roots::global_persistent_handle_table().get(task.promise) {
      maybe_clear_external_pending(promise_ptr.cast::<PromiseHeader>());
    }

    // Free the temporary payload buffer.
    if task.layout.size != 0 && !task.tmp_payload.is_null() {
      let align = task.layout.align.max(1);
      if !align.is_power_of_two() {
        std::process::abort();
      }
      let layout = Layout::from_size_align(task.layout.size, align).unwrap_or_else(|_| std::process::abort());
      unsafe {
        std::alloc::dealloc(task.tmp_payload, layout);
      }
    }
  }

  let _ = crate::roots::global_persistent_handle_table().free(task.promise);
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

  pub(crate) fn spawn(
    &self,
    task: extern "C" fn(*mut u8, LegacyPromiseRef),
    data: *mut u8,
  ) -> LegacyPromiseRef {
    // Ensure the async runtime is initialized so promise settlement can wake a thread blocked in the
    // platform reactor wait syscall (`epoll_wait`/`kevent`).
    let _ = async_rt::global();
    let promise = async_rt::promise::promise_new();

    {
      let mut q = self.shared.queue.lock();
      q.push_back(WorkItem {
        data: WorkData::Unrooted(data),
        kind: WorkKind::Legacy { task, promise },
      });
    }
    self.shared.cv.notify_one();
    promise
  }

  fn spawn_promise_impl(&self, task: extern "C" fn(*mut u8, *mut u8) -> u8, data: WorkData, layout: PromiseLayout) -> PromiseRef {
    // Ensure the async runtime is initialized so microtask settlement can wake a blocked
    // `epoll_wait`.
    let _ = async_rt::global();

    // Allocate a GC-managed payload promise (out-of-line payload buffer). The external-pending flag
    // is cleared and the counter decremented by `rt_promise_{fulfill,reject}`.
    let promise = crate::payload_promise::alloc_payload_promise(layout, true);

    // Keep the promise object alive (and relocatable) while the blocking task is outstanding. Even
    // if the caller drops the returned `PromiseRef` immediately, the blocking task still needs to
    // produce the payload bytes and the runtime needs to settle the promise.
    //
    // Use a temporary handle-stack root so `alloc_from_slot` reads the promise pointer after
    // acquiring its lock (moving-GC safe under lock contention).
    let promise_handle = {
      let tmp = crate::roots::Root::<u8>::new(promise.0.cast::<u8>());
      // Safety: `tmp.handle()` is a valid pointer-to-slot (`GcHandle`) containing a GC object base
      // pointer.
      unsafe { crate::roots::global_persistent_handle_table().alloc_from_slot(tmp.handle()) }
      // `tmp` dropped here, removing the handle-stack root before any further potentially-blocking
      // operations (e.g. queue lock acquisition).
    };

    // Allocate a temporary non-GC payload buffer for the blocking task to write into.
    //
    // The buffer is copied into the promise payload on the event-loop thread when settling.
    let tmp_payload = if layout.size == 0 {
      core::ptr::null_mut()
    } else {
      let align = layout.align.max(1);
      if !align.is_power_of_two() {
        trap::rt_trap_invalid_arg("promise payload align must be a power of two");
      }
      let buf_layout =
        Layout::from_size_align(layout.size, align).unwrap_or_else(|_| trap::rt_trap_invalid_arg("promise payload layout"));
      let ptr = unsafe { std::alloc::alloc_zeroed(buf_layout) };
      if ptr.is_null() {
        trap::rt_trap_oom(layout.size, "blocking promise temp payload");
      }
      ptr
    };

    {
      let mut q = self.shared.queue.lock();
      q.push_back(WorkItem {
        data,
        kind: WorkKind::Promise {
          task,
          promise: promise_handle,
          tmp_payload,
          layout,
        },
      });
    }
    self.shared.cv.notify_one();

    let promise_ptr = crate::roots::global_persistent_handle_table()
      .get(promise_handle)
      .unwrap_or_else(|| std::process::abort());
    PromiseRef(promise_ptr.cast())
  }

  pub(crate) fn spawn_promise(
    &self,
    task: extern "C" fn(*mut u8, *mut u8) -> u8,
    data: *mut u8,
    layout: PromiseLayout,
  ) -> PromiseRef {
    self.spawn_promise_impl(task, WorkData::Unrooted(data), layout)
  }

  pub(crate) fn spawn_promise_rooted(
    &self,
    task: extern "C" fn(*mut u8, *mut u8) -> u8,
    data: *mut u8,
    layout: PromiseLayout,
  ) -> PromiseRef {
    // Safety: caller must uphold the rooted-task contract that `data` is the base pointer of a
    // GC-managed object.
    let root = unsafe { async_rt::gc::Root::new_unchecked(data) };
    self.spawn_promise_impl(task, WorkData::Rooted(root), layout)
  }

  pub(crate) unsafe fn spawn_promise_rooted_h(
    &self,
    task: extern "C" fn(*mut u8, *mut u8) -> u8,
    slot: crate::roots::GcHandle,
    layout: PromiseLayout,
  ) -> PromiseRef {
    // Safety: caller must uphold the rooted-task contract that `slot` is a valid pointer to a
    // writable `GcPtr` slot containing the base pointer of a GC-managed object.
    let root = unsafe { async_rt::gc::Root::new_from_slot_unchecked(slot) };
    self.spawn_promise_impl(task, WorkData::Rooted(root), layout)
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

    let WorkItem { data, kind } = work;
    match kind {
      WorkKind::Legacy { task, promise } => {
        let data = match &data {
          WorkData::Unrooted(ptr) => *ptr,
          WorkData::Rooted(root) => root.ptr(),
        };

        // Blocking tasks execute in a GC-safe region so stop-the-world GC doesn't deadlock on a
        // worker thread blocked in a syscall or long wait.
        //
        // Contract: the task must not touch or mutate the GC heap while running (no GC allocations,
        // no dereferencing GC pointers, no write barriers).
        let gc_safe = threading::enter_gc_safe_region();

        // The task is responsible for settling the legacy promise. If it panics we abort the
        // process deterministically instead of unwinding into the runtime.
        crate::ffi::invoke_cb2_legacy_promise(task, data, promise);

        drop(gc_safe);
      }

      WorkKind::Promise {
        task,
        promise,
        tmp_payload,
        layout,
      } => {
        let data = match &data {
          WorkData::Unrooted(ptr) => *ptr,
          WorkData::Rooted(root) => root.ptr(),
        };

        let gc_safe = threading::enter_gc_safe_region();

        let tag = crate::ffi::invoke_cb2_blocking_promise_task(task, data, tmp_payload);

        // Hop back to the event-loop thread to settle the GC-managed promise.
        let micro = Box::new(BlockingPromiseMicrotask {
          promise,
          tag,
          tmp_payload,
          layout,
          ran: false,
        });
        async_rt::global().enqueue_microtask(async_rt::Task::new_with_drop(
          run_blocking_promise_microtask,
          Box::into_raw(micro) as *mut u8,
          drop_blocking_promise_microtask,
        ));

        drop(gc_safe);
      }
    }
  }
}
