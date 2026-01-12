use std::alloc::handle_alloc_error;
use std::alloc::Layout;
use std::collections::VecDeque;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use ahash::AHashSet;

use super::roots::RememberedSet;
use super::roots::RootSet;
use super::work_stack::WorkStack;
use super::weak::process_global_weak_handles_major;
use super::weak::run_weak_cleanups;
use super::Tracer;
use crate::gc::heap::AllocError;
use crate::gc::heap::GcHeap;
use crate::gc::heap::IMMIX_BLOCK_SIZE;
use crate::gc::heap::IMMIX_LINES_PER_BLOCK;
use crate::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use crate::immix::BumpCursor;

impl GcHeap {
  /// Perform a full-heap major collection using a mark-region algorithm over
  /// Immix blocks plus sweeping of the large-object space.
  ///
  /// This method begins with a [`GcHeap::collect_minor`] to ensure no nursery
  /// objects remain.
  ///
  /// # Stop-the-world requirement
  /// This GC is **stop-the-world**: the caller must ensure there are no
  /// concurrent mutators and that the provided root/remembered sets remain
  /// stable for the duration of the call.
  pub fn collect_major(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
  ) -> Result<(), AllocError> {
    self.collect_major_with_mark_workers_opt(roots, remembered, None)
  }

  /// Like [`GcHeap::collect_major`], but allows explicitly controlling the
  /// number of threads used for the stop-the-world marking phase.
  ///
  /// This is a test/debug hook; most callers should prefer [`GcHeap::collect_major`].
  #[doc(hidden)]
  pub fn collect_major_with_mark_workers(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
    mark_workers: usize,
  ) -> Result<(), AllocError> {
    self.collect_major_with_mark_workers_opt(roots, remembered, Some(mark_workers))
  }

  fn collect_major_with_mark_workers_opt(
    &mut self,
    roots: &mut dyn RootSet,
    remembered: &mut dyn RememberedSet,
    mark_workers: Option<usize>,
  ) -> Result<(), AllocError> {
    if !super::gc_in_progress() {
      // Major GC begins with a minor GC which may install per-object card tables
      // for promoted large pointer arrays. Ensure the registry has enough spare
      // capacity before entering the GC-in-progress state.
      self.reserve_card_table_objects_for_minor_gc();
    }
    let _gc_guard = super::GcInProgressGuard::new();
    self.collect_minor(roots, remembered)?;
    self.stats.major_collections += 1;
    let start = Instant::now();

    // Toggle the epoch so we can treat previous marks as "unmarked" without
    // clearing every object header.
    self.mark_epoch ^= 1;
    let epoch = self.mark_epoch;

    // Reset all Immix liveness maps.
    self.immix.clear_line_marks();

    let mark_workers = mark_workers.unwrap_or_else(|| {
      let cfg = self.config().major_gc_mark_threads;
      if cfg == 0 { parallel_marker_pool().max_workers } else { cfg }
    });
    parallel_mark_major(self, epoch, roots, mark_workers);

    let cfg = self.major_compaction;
    if cfg.enabled && cfg.max_live_ratio_percent <= 100 {
      // Major compaction is optional and disabled by default. Avoid allocating the candidate bitmap
      // unless we actually find candidate blocks.
      let candidate_blocks_opt = {
        let immix = &self.immix;

        let is_candidate_block = |block_id: usize| -> bool {
          let Some(metrics) = immix.block_metrics(block_id) else {
            return false;
          };

          let live_lines = IMMIX_LINES_PER_BLOCK - metrics.free_lines;
          if live_lines == 0 {
            return false;
          }
          if live_lines < cfg.min_live_lines {
            return false;
          }

          live_lines * 100 < cfg.max_live_ratio_percent as usize * IMMIX_LINES_PER_BLOCK
        };

        let mut candidate_count = 0usize;
        for block_id in 0..immix.block_count() {
          if is_candidate_block(block_id) {
            candidate_count += 1;
          }
        }

        if candidate_count == 0 {
          None
        } else {
          let mut candidate_blocks = vec![false; immix.block_count()];
          for block_id in 0..candidate_blocks.len() {
            if is_candidate_block(block_id) {
              candidate_blocks[block_id] = true;
            }
          }
          Some(candidate_blocks)
        }
      };

      if let Some(candidate_blocks) = candidate_blocks_opt {
        // Rebuild the Immix availability structure so evacuation can allocate
        // into existing holes (opportunistic copying) instead of always growing
        // the heap with new blocks.
        //
        // We will rebuild again after compaction since clearing candidate blocks
        // and allocating new objects changes hole sizes.
        self.immix.finalize_after_marking();

        let mut pinned_in_candidates: Vec<Vec<*mut u8>> = vec![Vec::new(); candidate_blocks.len()];

        {
          let mut compactor = Compactor {
            heap: self,
            candidate_blocks: &candidate_blocks,
            pinned_in_candidates: &mut pinned_in_candidates,
            worklist: VecDeque::new(),
            visited: AHashSet::new(),
            bump: BumpCursor::new(),
          };

          roots.for_each_root_slot(&mut |slot| {
            compactor.visit_slot(slot);
          });

          crate::roots::global_root_registry().for_each_root_slot(|slot| compactor.visit_slot(slot));
          crate::roots::global_persistent_handle_table()
            .for_each_root_slot(|slot| compactor.visit_slot(slot));

          let mut root_handles = mem::take(&mut compactor.heap.root_handles);
          root_handles.for_each_root_slot(&mut |slot| {
            compactor.visit_slot(slot);
          });
          compactor.heap.root_handles = root_handles;

          while let Some(obj) = compactor.worklist.pop_front() {
            compactor.visit_obj(obj);
          }
        }

        for (block_id, is_candidate) in candidate_blocks.iter().enumerate() {
          if *is_candidate {
            self.immix.clear_block_line_map(block_id);
            // If we encountered pinned objects in a candidate block, we cannot
            // evacuate them. Re-mark their lines so they remain live and the
            // block is not treated as fully free.
            for &pinned in &pinned_in_candidates[block_id] {
              unsafe {
                let size = super::obj_size(pinned);
                self.immix.set_lines_for_live_object(pinned, size);
              }
            }
          }
        }
      }
    }

    self.process_weak_handles_major(epoch);
    process_global_weak_handles_major(self, epoch);
    run_weak_cleanups(self);
    self.process_finalizers_major(epoch);
    self.sweep_card_table_objects_major(epoch);
    self.stats.last_major_live_bytes = self.immix.line_map_used_bytes() + self.los.live_bytes(epoch);
    self.immix.finalize_after_marking();
    self.los.sweep(epoch);
    crate::rt_alloc::bump_major_epoch();

    let pause = start.elapsed();
    self.stats.last_major_pause = pause;
    self.stats.total_major_pause += pause;
    Ok(())
  }
}

// -------------------------------------------------------------------------------------------------
// Parallel major-GC marking (stop-the-world)
// -------------------------------------------------------------------------------------------------

static PARALLEL_MARK_POOL: OnceLock<Arc<ParallelMarkPool>> = OnceLock::new();

pub(super) fn ensure_parallel_marker_pool_init() {
  let _ = parallel_marker_pool();
}

fn parallel_marker_pool() -> &'static ParallelMarkPool {
  PARALLEL_MARK_POOL
    .get_or_init(|| ParallelMarkPool::new())
    .as_ref()
}

struct ParallelMarkPool {
  /// Max workers for marking, including the GC coordinator thread.
  max_workers: usize,
  /// Serialize marking jobs: a single pool is shared across all heaps in the process.
  job_lock: Mutex<()>,
  state: Mutex<PoolState>,
  start_cv: Condvar,
  done_cv: Condvar,

  pending: AtomicUsize,
  global: Mutex<WorkStack>,
  worker0: Mutex<WorkStack>,
}

struct PoolState {
  gen: u64,
  active_workers: usize,
  heap: usize,
  epoch: u8,
  done: usize,
}

impl ParallelMarkPool {
  fn new() -> Arc<Self> {
    let max_workers = parallel_mark_worker_count();

    // Use the same sizing heuristic as the single-threaded work stack (env var + 64MB default) for
    // the shared/global queue. Per-worker local stacks are sized relative to that.
    let global = WorkStack::new();
    let global_bytes = global.capacity().saturating_mul(mem::size_of::<*mut u8>());
    // Keep per-worker stacks reasonably small so enabling parallel marking doesn't multiply the
    // reserved address space by `workers`.
    let local_bytes = (global_bytes / max_workers.saturating_mul(2).max(1)).clamp(256 * 1024, 8 * 1024 * 1024);

    let pool = Arc::new(Self {
      max_workers,
      job_lock: Mutex::new(()),
      state: Mutex::new(PoolState {
        gen: 0,
        active_workers: 0,
        heap: 0,
        epoch: 0,
        done: 0,
      }),
      start_cv: Condvar::new(),
      done_cv: Condvar::new(),
      pending: AtomicUsize::new(0),
      global: Mutex::new(global),
      worker0: Mutex::new(WorkStack::with_capacity_bytes(local_bytes)),
    });

    // `no_alloc_rt_gc_collect` integration tests assert that the first stop-the-world GC does not
    // allocate via the Rust global allocator.
    //
    // Spawning threads here ensures we don't try to create worker threads while the world is
    // stopped. However, OS thread start-up (TLS initialization, libc bookkeeping, etc.) can perform
    // allocations *on the worker threads* asynchronously after `spawn` returns. If those happen to
    // race with the first `rt_gc_collect`, the test's global allocator hook can observe them.
    //
    // Wait for all mark worker threads to enter their event loops before returning so any one-time
    // start-up allocations happen during heap initialization / `rt_thread_init`, not during the
    // first GC cycle.
    let ready = if max_workers > 1 {
      Some(Arc::new(Barrier::new(max_workers)))
    } else {
      None
    };

    for worker_id in 1..max_workers {
      let pool = Arc::clone(&pool);
      let local = WorkStack::with_capacity_bytes(local_bytes);
      let ready = ready.as_ref().map(Arc::clone);
      let name = format!("rt-gc-mark-{worker_id}");
      let _ = thread::Builder::new()
        .name(name)
        .spawn(move || {
          if let Some(ready) = ready {
            let _ = ready.wait();
          }
          crate::ffi::abort_on_panic(|| mark_worker_loop(worker_id, pool, local))
        })
        .unwrap_or_else(|_| std::process::abort());
    }

    if let Some(ready) = ready {
      let _ = ready.wait();
    }

    pool
  }
}

fn parallel_mark_worker_count() -> usize {
  // Allow configuring GC marking threads separately, but fall back to the runtime's
  // general worker-pool thread count env var for convenience.
  //
  // Keep debug builds conservative to avoid over-subscribing test hosts.
  let from_env = std::env::var("ECMA_RS_RUNTIME_NATIVE_GC_THREADS")
    .ok()
    .or_else(|| std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS").ok())
    .or_else(|| std::env::var("RT_NUM_THREADS").ok())
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|&n| n > 0);

  let default = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
  let n = from_env.unwrap_or(default);
  let n = if cfg!(debug_assertions) { n.min(32) } else { n };
  n.max(1)
}

fn parallel_mark_major(heap: &mut GcHeap, epoch: u8, roots: &mut dyn RootSet, mark_workers: usize) {
  let pool = parallel_marker_pool();
  let workers = mark_workers.clamp(1, pool.max_workers);

  // Only one heap may use the mark pool at a time; collect_major is stop-the-world, but tests may
  // run multiple independent heaps/collections concurrently.
  let _job_guard = pool
    .job_lock
    .lock()
    .unwrap_or_else(|_| std::process::abort());

  // Reset shared state for this cycle.
  pool.pending.store(0, Ordering::Release);
  {
    let mut global = pool.global.lock().unwrap_or_else(|_| std::process::abort());
    global.clear();

    // Seed the global queue from roots. Root enumeration happens on the coordinator thread before
    // any worker threads start, so we can hold the global queue lock for the duration.
    roots.for_each_root_slot(&mut |slot| unsafe {
      let obj = *slot;
      mark_obj_enqueue_global(&*heap, epoch, obj, &pool.pending, &mut global);
    });

    // Process-global roots/handles registered outside of stackmaps (intern tables, runtime-owned
    // queues, host handles, ...).
    crate::roots::global_root_registry().for_each_root_slot(|slot| unsafe {
      mark_obj_enqueue_global(&*heap, epoch, *slot, &pool.pending, &mut global);
    });
    crate::roots::global_persistent_handle_table().for_each_root_slot(|slot| unsafe {
      mark_obj_enqueue_global(&*heap, epoch, *slot, &pool.pending, &mut global);
    });

    let mut root_handles = mem::take(&mut heap.root_handles);
    root_handles.for_each_root_slot(&mut |slot| unsafe {
      mark_obj_enqueue_global(&*heap, epoch, *slot, &pool.pending, &mut global);
    });
    heap.root_handles = root_handles;
  }

  // Publish the job and wake workers.
  {
    let mut state = pool.state.lock().unwrap_or_else(|_| std::process::abort());
    state.gen = state.gen.wrapping_add(1);
    state.active_workers = workers;
    state.heap = heap as *const GcHeap as usize;
    state.epoch = epoch;
    state.done = 0;
    pool.start_cv.notify_all();
  }

  // Coordinator participates as worker 0.
  {
    let heap_ptr: *const GcHeap = heap as *const GcHeap;
    let heap_ref = unsafe { &*heap_ptr };
    let mut local = pool.worker0.lock().unwrap_or_else(|_| std::process::abort());
    local.clear();
    run_mark_worker(heap_ref, epoch, &pool.pending, &pool.global, &mut local);
  }

  // Wait for all participating background workers to finish.
  if workers > 1 {
    let mut state = pool.state.lock().unwrap_or_else(|_| std::process::abort());
    while state.done < workers - 1 {
      state = pool.done_cv.wait(state).unwrap_or_else(|_| std::process::abort());
    }
  }

  debug_assert_eq!(
    pool.pending.load(Ordering::Acquire),
    0,
    "parallel major GC marking ended with pending work"
  );
}

fn mark_worker_loop(worker_id: usize, pool: Arc<ParallelMarkPool>, mut local: WorkStack) -> ! {
  let mut last_gen: u64 = 0;
  loop {
    // Wait for a new marking job.
    let (gen, active_workers, heap, epoch) = {
      let mut state = pool.state.lock().unwrap_or_else(|_| std::process::abort());
      while state.gen == last_gen {
        state = pool.start_cv.wait(state).unwrap_or_else(|_| std::process::abort());
      }
      last_gen = state.gen;
      (state.gen, state.active_workers, state.heap, state.epoch)
    };

    // Worker threads are created up-front, but each GC cycle may use fewer workers than the pool.
    if worker_id >= active_workers {
      continue;
    }

    // Safety: the coordinator keeps the heap pointer valid for the duration of the job and
    // serializes jobs via `job_lock`.
    let heap = unsafe { &*(heap as *const GcHeap) };
    local.clear();
    run_mark_worker(heap, epoch, &pool.pending, &pool.global, &mut local);

    // Report completion.
    let mut state = pool.state.lock().unwrap_or_else(|_| std::process::abort());
    if state.gen != gen {
      // A new GC cycle started unexpectedly while we were marking. This should be impossible due to
      // `job_lock`, but fail fast rather than corrupting GC state.
      std::process::abort();
    }
    state.done += 1;
    pool.done_cv.notify_all();
  }
}

const STEAL_BATCH: usize = 256;
const SHARE_THRESHOLD: usize = 1024;

fn run_mark_worker(
  heap: &GcHeap,
  epoch: u8,
  pending: &AtomicUsize,
  global: &Mutex<WorkStack>,
  local: &mut WorkStack,
) {
  let mut worker = MarkWorker {
    heap,
    epoch,
    pending,
    global,
    local,
  };
  worker.run();
}

struct MarkWorker<'a> {
  heap: &'a GcHeap,
  epoch: u8,
  pending: &'a AtomicUsize,
  global: &'a Mutex<WorkStack>,
  local: &'a mut WorkStack,
}

impl MarkWorker<'_> {
  fn run(&mut self) {
    loop {
      if let Some(obj) = self.local.pop() {
        self.visit_obj(obj);
        self.pending.fetch_sub(1, Ordering::AcqRel);
        self.maybe_share();
        continue;
      }

      if self.steal_from_global() {
        continue;
      }

      if self.pending.load(Ordering::Acquire) == 0 {
        return;
      }

      std::hint::spin_loop();
      std::thread::yield_now();
    }
  }

  fn steal_from_global(&mut self) -> bool {
    let mut global = self.global.lock().unwrap_or_else(|_| std::process::abort());
    let mut n = 0usize;
    while n < STEAL_BATCH {
      let Some(obj) = global.pop() else { break };
      self.local.push(obj);
      n += 1;
    }
    n != 0
  }

  fn maybe_share(&mut self) {
    let len = self.local.len();
    if len < SHARE_THRESHOLD {
      return;
    }

    let share = len / 2;
    if share == 0 {
      return;
    }

    let mut global = self.global.lock().unwrap_or_else(|_| std::process::abort());
    for _ in 0..share {
      let Some(obj) = self.local.pop() else { break };
      global.push(obj);
    }
  }

  fn mark_obj(&mut self, obj: *mut u8) {
    mark_obj_enqueue_local(self.heap, self.epoch, obj, self.pending, self.local);
    self.maybe_share();
  }
}

impl Tracer for MarkWorker<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let obj = unsafe { *slot };
    self.mark_obj(obj);
  }
}

fn mark_obj_enqueue_global(
  heap: &GcHeap,
  epoch: u8,
  mut obj: *mut u8,
  pending: &AtomicUsize,
  global: &mut WorkStack,
) {
  if obj.is_null() {
    return;
  }

  // `collect_major` runs `collect_minor` first, so in the common case there should be no nursery
  // pointers left. Handle them (and any stale/foreign pointers) defensively anyway.
  loop {
    if !heap.is_valid_obj_ptr_for_tracing(obj, true) {
      return;
    }

    if heap.is_in_nursery(obj) {
      // SAFETY: `obj` is a valid pointer into this heap's nursery.
      unsafe {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          continue;
        }
      }
      return;
    }

    // Follow forwarding pointers (used by nursery evacuation today, and by potential future major
    // GC compaction).
    // SAFETY: `obj` is in this heap (Immix or LOS), so it points at an `ObjHeader`.
    unsafe {
      let header = &*super::header_from_obj(obj);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
        continue;
      }
    }

    break;
  }

  // SAFETY: `obj` points to an `ObjHeader`.
  let first_mark = unsafe { (&*super::header_from_obj(obj)).set_mark_epoch_idempotent(epoch) };
  if !first_mark {
    return;
  }

  // SAFETY: `obj` points to a valid object header.
  let size = unsafe { super::obj_size(obj) };
  if heap.is_in_immix(obj) {
    heap.immix.set_lines_for_live_object(obj, size);
  } else {
    debug_assert!(heap.is_in_los(obj), "unknown heap object location");
  }

  pending.fetch_add(1, Ordering::Relaxed);
  global.push(obj);
}

fn mark_obj_enqueue_local(
  heap: &GcHeap,
  epoch: u8,
  mut obj: *mut u8,
  pending: &AtomicUsize,
  local: &mut WorkStack,
) {
  if obj.is_null() {
    return;
  }

  // `collect_major` runs `collect_minor` first, so in the common case there should be no nursery
  // pointers left. Handle them (and any stale/foreign pointers) defensively anyway.
  loop {
    if !heap.is_valid_obj_ptr_for_tracing(obj, true) {
      return;
    }

    if heap.is_in_nursery(obj) {
      // SAFETY: `obj` is a valid pointer into this heap's nursery.
      unsafe {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          continue;
        }
      }
      return;
    }

    // Follow forwarding pointers (used by nursery evacuation today, and by potential future major
    // GC compaction).
    // SAFETY: `obj` is in this heap (Immix or LOS), so it points at an `ObjHeader`.
    unsafe {
      let header = &*super::header_from_obj(obj);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
        continue;
      }
    }

    break;
  }

  // SAFETY: `obj` points to an `ObjHeader`.
  let first_mark = unsafe { (&*super::header_from_obj(obj)).set_mark_epoch_idempotent(epoch) };
  if !first_mark {
    return;
  }

  // SAFETY: `obj` points to a valid object header.
  let size = unsafe { super::obj_size(obj) };
  if heap.is_in_immix(obj) {
    heap.immix.set_lines_for_live_object(obj, size);
  } else {
    debug_assert!(heap.is_in_los(obj), "unknown heap object location");
  }

  pending.fetch_add(1, Ordering::Relaxed);
  local.push(obj);
}

struct Compactor<'a> {
  heap: &'a mut GcHeap,
  candidate_blocks: &'a [bool],
  pinned_in_candidates: &'a mut [Vec<*mut u8>],
  worklist: VecDeque<*mut u8>,
  visited: AHashSet<usize>,
  bump: BumpCursor,
}

impl Compactor<'_> {
  fn enqueue_obj(&mut self, obj: *mut u8) -> bool {
    if obj.is_null() {
      return false;
    }
    if !self.visited.insert(obj as usize) {
      return false;
    }
    self.worklist.push_back(obj);
    true
  }

  fn candidate_block_id(&self, obj: *mut u8) -> Option<usize> {
    if !self.heap.is_in_immix(obj) {
      return None;
    }
    let Some(block_id) = self.heap.immix.block_id_for_ptr(obj) else {
      return None;
    };
    if self.candidate_blocks.get(block_id).copied().unwrap_or(false) {
      Some(block_id)
    } else {
      None
    }
  }

  fn alloc_to_space(&mut self, size: usize, align: usize) -> *mut u8 {
    debug_assert!(align != 0 && align.is_power_of_two());
    let obj = if size > IMMIX_MAX_OBJECT_SIZE || align > IMMIX_BLOCK_SIZE {
      self.heap.los.alloc(size, align)
    } else {
      self
        .heap
        .immix
        .alloc_old_with_cursor_excluding(&mut self.bump, size, align, self.candidate_blocks)
        .unwrap_or_else(|| handle_alloc_error(Layout::from_size_align(size, align).unwrap()))
    };

    self.heap.stats.bytes_allocated_old += size;
    obj
  }

  fn evacuate(&mut self, obj: *mut u8) -> *mut u8 {
    debug_assert!(self.candidate_block_id(obj).is_some());

    // SAFETY: `obj` is expected to be a valid heap object.
    unsafe {
      let header_ptr = super::header_from_obj(obj);
      if (*header_ptr).is_forwarded() {
        return (*header_ptr).forwarding_ptr();
      }
      if (*header_ptr).is_pinned() {
        return obj;
      }

      let desc = (*header_ptr).type_desc();
      let size = super::obj_size(obj);
      let new_obj = self.alloc_to_space(size, desc.align);
      ptr::copy_nonoverlapping(obj, new_obj, size);
      (*header_ptr).set_forwarding_ptr(new_obj);
      new_obj
    }
  }
}

impl Tracer for Compactor<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let mut obj = unsafe { *slot };
    if obj.is_null() {
      return;
    }

    // `collect_major` runs `collect_minor` first, so in the common case there should be no nursery
    // pointers left. Handle them (and any stale/foreign pointers) defensively anyway.
    loop {
      if !self.heap.is_valid_obj_ptr_for_tracing(obj, true) {
        return;
      }

      if self.heap.is_in_nursery(obj) {
        // SAFETY: `obj` is a valid pointer into this heap's nursery.
        unsafe {
          let header = &*super::header_from_obj(obj);
          if header.is_forwarded() {
            obj = header.forwarding_ptr();
            // SAFETY: `slot` is valid and writable.
            *slot = obj;
            continue;
          }
        }
        return;
      }

      // Follow forwarding pointers (objects already evacuated from candidate blocks) and update the
      // slot.
      // SAFETY: `obj` is in this heap (Immix or LOS), so it points at an `ObjHeader`.
      unsafe {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          // SAFETY: `slot` is valid and writable.
          *slot = obj;
          continue;
        }
      }

      break;
    }

    let mut pinned_block_id: Option<usize> = None;
    if let Some(block_id) = self.candidate_block_id(obj) {
      // Pinned objects must remain in place; remember them so we can re-mark
      // their lines after clearing the candidate block.
      // SAFETY: `obj` is expected to be a valid heap object.
      if unsafe { (&*super::header_from_obj(obj)).is_pinned() } {
        pinned_block_id = Some(block_id);
      } else {
        let new_obj = self.evacuate(obj);
        if new_obj != obj {
          // SAFETY: `slot` is valid and writable.
          unsafe {
            *slot = new_obj;
          }
          obj = new_obj;
        }
      }
    }

    let first_visit = self.enqueue_obj(obj);
    if first_visit {
      if let Some(block_id) = pinned_block_id {
        self.pinned_in_candidates[block_id].push(obj);
      }
    }
  }
}
