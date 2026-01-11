use crate::abi::PromiseRef;
use crate::abi::RtCoroutineHeader;
use crate::abi::RtShapeId;
use crate::abi::TaskId;
use crate::abi::TimerId;
use crate::abi::ValueRef;
use crate::abi::IoWatcherId;
use crate::alloc;
use crate::array;
use crate::array::RtArrayHeader;
use crate::async_rt;
use crate::async_rt::WatcherId;
use crate::gc::ObjHeader;
use crate::gc::SimpleRememberedSet;
use crate::gc::TypeDescriptor;
use crate::gc::WeakHandle;
use crate::gc::YOUNG_SPACE;
use crate::BackingStoreAllocator;
use crate::shape_table;
use crate::threading;
use crate::threading::registry;
use crate::Runtime;
use crate::Thread;
use std::panic::{catch_unwind, AssertUnwindSafe};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[inline(always)]
fn ensure_event_loop_thread_registered() {
  // The async runtime is driven by the main thread/event loop. Register it on
  // first use so GC can coordinate stop-the-world safepoints across all
  // mutator threads.
  crate::threading::register_current_thread(crate::threading::ThreadKind::Main);
}

#[no_mangle]
pub extern "C" fn rt_alloc(size: usize, shape: RtShapeId) -> *mut u8 {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc(size);

  // Don't let panics unwind across the extern "C" boundary.
  let res = catch_unwind(AssertUnwindSafe(|| {
    let desc = shape_table::lookup_rt_descriptor(shape);
    if size != desc.size as usize {
      crate::trap::rt_trap_invalid_arg("rt_alloc: size does not match registered shape descriptor");
    }

    let align = desc.align as usize;
    let obj = alloc::alloc_bytes(size, align, "rt_alloc");

    // Ensure pointer slots start out as null so tracing never sees uninitialized garbage.
    // SAFETY: `obj` is valid for `size` bytes.
    unsafe {
      std::ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = shape_table::lookup_type_descriptor(shape) as *const _;
      header.meta = 0;
    }

    obj
  }));

  match res {
    Ok(ptr) => ptr,
    Err(_) => std::process::abort(),
  }
}

/// Allocate a pinned (non-moving) object.
///
/// NOTE: The milestone runtime does not yet wire allocations into the GC. This entrypoint exists so
/// codegen/FFI can request a stable address today and so future GC-backed allocation can route
/// pinned objects to a non-moving space.
#[no_mangle]
pub extern "C" fn rt_alloc_pinned(size: usize, shape: RtShapeId) -> *mut u8 {
  // Don't let panics unwind across the extern "C" boundary.
  let res = catch_unwind(AssertUnwindSafe(|| {
    let desc = shape_table::lookup_rt_descriptor(shape);
    if size != desc.size as usize {
      crate::trap::rt_trap_invalid_arg(
        "rt_alloc_pinned: size does not match registered shape descriptor",
      );
    }

    let align = desc.align as usize;
    let obj = alloc::alloc_bytes(size, align, "rt_alloc_pinned");

    // SAFETY: `obj` is valid for `size` bytes.
    unsafe {
      std::ptr::write_bytes(obj, 0, size);
      let header = &mut *(obj as *mut ObjHeader);
      header.type_desc = shape_table::lookup_type_descriptor(shape) as *const _;
      header.meta = 0;
      header.set_pinned(true);
    }

    obj
  }));

  match res {
    Ok(ptr) => ptr,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> *mut u8 {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc_array(len, elem_size);

  let Some(spec) = array::decode_rt_array_elem_size(elem_size) else {
    crate::trap::rt_trap_invalid_arg("rt_alloc_array: invalid elem_size");
  };
  let size = array::checked_total_bytes(len, spec.elem_size)
    .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("rt_alloc_array: size overflow"));

  let obj = alloc::alloc_bytes_zeroed(size, 16, "rt_alloc_array");
  // SAFETY: `obj` points to `size` bytes of writable, zeroed memory.
  unsafe {
    let header = &mut *(obj as *mut ObjHeader);
    header.type_desc = &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor;
    header.meta = 0;

    let arr = &mut *(obj as *mut RtArrayHeader);
    arr.len = len;
    arr.elem_size = spec.elem_size as u32;
    arr.elem_flags = spec.elem_flags;
  }

  obj
}

/// Register the current OS thread with the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_init(kind: u32) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_init();

  let kind = match kind {
    0 => threading::ThreadKind::Main,
    1 => threading::ThreadKind::Worker,
    2 => threading::ThreadKind::Io,
    3 => threading::ThreadKind::External,
    _ => threading::ThreadKind::External,
  };
  threading::register_current_thread(kind);
}

/// Unregister the current OS thread from the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_deinit() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_deinit();
  threading::unregister_current_thread();
}

/// GC safepoint.
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_safepoint();

  // `rt_gc_safepoint` is only meaningful for threads that have been registered
  // with `rt_thread_init`. For non-attached threads we treat this as a no-op in
  // release builds (and assert in debug builds).
  if registry::current_thread_id().is_none() {
    debug_assert!(
      false,
      "rt_gc_safepoint called from a thread that is not registered (rt_thread_init was not called)"
    );
    return;
  }

  crate::threading::safepoint::rt_gc_safepoint();
}

/// Cheap leaf poll used by compiler-inserted loop backedge safepoints.
///
/// Returns `true` if a stop-the-world safepoint is currently requested.
#[no_mangle]
pub extern "C" fn rt_gc_poll() -> bool {
  crate::threading::safepoint::rt_gc_poll()
}

/// LLVM `place-safepoints` poll function.
///
/// LLVM's `place-safepoints` pass inserts calls to a symbol named
/// `gc.safepoint_poll` in functions that use a statepoint-based GC strategy.
/// Those calls are later rewritten into statepoints by `rewrite-statepoints-for-gc`.
///
/// We export this symbol from the runtime so codegen can use `place-safepoints`
/// without needing to synthesize its own poll function body in every module.
#[export_name = "gc.safepoint_poll"]
pub extern "C" fn rt_gc_safepoint_poll() {
  rt_gc_safepoint();
}

/// Update the active young-space address range used by the write barrier.
///
/// This must be called by the GC during initialization and after each nursery
/// flip/resize that changes the current young generation region.
#[no_mangle]
pub extern "C" fn rt_gc_set_young_range(start: *mut u8, end: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_set_young_range();
  YOUNG_SPACE.start.store(start as usize, Ordering::Release);
  YOUNG_SPACE.end.store(end as usize, Ordering::Release);
}

/// Debug/test helper: return the current young-space range.
///
/// # Safety
/// If `out_start`/`out_end` are non-null, they must be valid writable pointers.
#[no_mangle]
pub unsafe extern "C" fn rt_gc_get_young_range(out_start: *mut *mut u8, out_end: *mut *mut u8) {
  if !out_start.is_null() {
    *out_start = YOUNG_SPACE.start.load(Ordering::Acquire) as *mut u8;
  }
  if !out_end.is_null() {
    *out_end = YOUNG_SPACE.end.load(Ordering::Acquire) as *mut u8;
  }
}

static REMEMBERED_SET: Lazy<Mutex<SimpleRememberedSet>> = Lazy::new(|| Mutex::new(SimpleRememberedSet::new()));

#[inline]
unsafe fn remember_old_object(obj: *mut u8) {
  debug_assert!(!obj.is_null());
  // Avoid taking the mutex in the common case where the object was already
  // recorded by a previous barrier hit.
  let header = &*(obj as *const ObjHeader);
  if header.is_remembered() {
    return;
  }
  REMEMBERED_SET.lock().remember(obj);
}

/// Write barrier for GC.
///
/// Records old→young pointer stores in the remembered set.
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier(obj: *mut u8, slot: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier();

  if obj.is_null() || slot.is_null() {
    return;
  }

  // Avoid UB on misaligned pointers: the barrier is specified to read a pointer-sized value from
  // `slot` and to treat `obj` as an `ObjHeader` base pointer.
  if (slot as usize) % std::mem::align_of::<*mut u8>() != 0 {
    std::process::abort();
  }
  if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
    std::process::abort();
  }

  // SAFETY: The write barrier contract requires `slot` be aligned and contain a
  // valid GC pointer or null.
  let value = (slot as *const *mut u8).read();
  if value.is_null() {
    return;
  }

  if !YOUNG_SPACE.contains(value as usize) {
    return;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find
  // the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  // Old → young store. Record the base object so minor GC can rescan it.
  remember_old_object(obj);
}

/// Range write barrier for GC.
///
/// Like [`rt_write_barrier`], but for a contiguous run of pointer slots.
///
/// - `start_slot` is the address of the first pointer slot.
/// - `len` is the number of bytes to scan (must cover a whole number of pointer slots).
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier_range(obj: *mut u8, start_slot: *mut u8, len: usize) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier_range();

  if obj.is_null() || start_slot.is_null() || len == 0 {
    return;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  let header = &*(obj as *const ObjHeader);
  if header.is_remembered() {
    return;
  }

  // Scan slots for any young pointer. If we see one, remember the object.
  let slot_count = len / core::mem::size_of::<*mut u8>();
  let slots = start_slot as *const *mut u8;
  for i in 0..slot_count {
    let value = slots.add(i).read();
    if value.is_null() {
      continue;
    }
    if YOUNG_SPACE.contains(value as usize) {
      remember_old_object(obj);
      return;
    }
  }
}

#[cfg(test)]
mod write_barrier_tests {
  use super::*;
  use crate::gc::roots::RememberedSet;

  #[repr(C)]
  struct DummyObject {
    header: ObjHeader,
    field: *mut u8,
  }

  fn clear_for_test() {
    REMEMBERED_SET.lock().clear();
    rt_gc_set_young_range(std::ptr::null_mut(), std::ptr::null_mut());
  }

  #[test]
  fn write_barrier_records_old_to_young_edges() {
    clear_for_test();

    let mut young_byte = Box::new(0u8);
    let young_ptr = (&mut *young_byte) as *mut u8;
    unsafe {
      rt_gc_set_young_range(young_ptr, young_ptr.add(1));
    }

    let mut old = Box::new(DummyObject {
      header: ObjHeader {
        type_desc: std::ptr::null(),
        meta: 0,
      },
      field: young_ptr,
    });

    let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
    let slot_ptr = (&mut old.field) as *mut *mut u8 as *mut u8;
    unsafe {
      rt_write_barrier(obj_ptr, slot_ptr);
    }

    assert!(old.header.is_remembered());
    assert!(REMEMBERED_SET.lock().contains(obj_ptr));

    clear_for_test();
  }

  #[test]
  fn write_barrier_range_records_old_to_young_edges() {
    clear_for_test();

    let mut young_byte = Box::new(0u8);
    let young_ptr = (&mut *young_byte) as *mut u8;
    unsafe {
      rt_gc_set_young_range(young_ptr, young_ptr.add(1));
    }

    #[repr(C)]
    struct DummyArray {
      header: ObjHeader,
      slots: [*mut u8; 4],
    }

    let mut old = Box::new(DummyArray {
      header: ObjHeader {
        type_desc: std::ptr::null(),
        meta: 0,
      },
      slots: [std::ptr::null_mut(); 4],
    });

    old.slots[2] = young_ptr;

    let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
    let start_slot = old.slots.as_mut_ptr() as *mut u8;
    let len = old.slots.len() * core::mem::size_of::<*mut u8>();
    unsafe {
      rt_write_barrier_range(obj_ptr, start_slot, len);
    }

    assert!(old.header.is_remembered());
    assert!(REMEMBERED_SET.lock().contains(obj_ptr));

    clear_for_test();
  }
}

/// Trigger a GC cycle.
///
/// Current milestone runtime:
/// - Performs a cooperative stop-the-world handshake across registered threads.
/// - Invokes the stackmap-based root enumeration hook (if stackmaps are available).
/// - Does *not* yet run a full GC algorithm (mark/copy/etc).
#[no_mangle]
pub extern "C" fn rt_gc_collect() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_gc_collect();

  let res = catch_unwind(AssertUnwindSafe(|| {
    // If a stop-the-world is already active, join it as a mutator safepoint at
    // this callsite (so we still publish a safepoint context for stack walking).
    let epoch = crate::threading::safepoint::current_epoch();
    if epoch & 1 == 1 {
      crate::safepoint::enter_safepoint_at_current_callsite(epoch);
      return;
    }

    // Attempt to become the stop-the-world coordinator.
    let Some(stop_epoch) = crate::threading::safepoint::rt_gc_try_request_stop_the_world() else {
      // Lost the race; if a GC is now active, join it.
      let epoch = crate::threading::safepoint::current_epoch();
      if epoch & 1 == 1 {
        crate::safepoint::enter_safepoint_at_current_callsite(epoch);
      }
      return;
    };

    // If `rt_gc_collect` is called from an attached mutator thread, publish the
    // initiator's safepoint context before waiting for other threads. This keeps
    // the initiator's stack eligible for stackmap-based root enumeration while
    // the world is stopped.
    if registry::current_thread_id().is_some() {
      let ctx = crate::arch::capture_safepoint_context();
      registry::set_current_thread_safepoint_context(ctx);
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);
      crate::threading::safepoint::notify_state_change();
    }

    crate::safepoint::with_world_stopped_requested(stop_epoch, || {});
  }));

  if res.is_err() {
    std::process::abort();
  }
}

/// Returns the total number of bytes currently held in non-moving backing stores (e.g. `ArrayBuffer`
/// bytes) allocated outside the GC heap.
///
/// This value is intended for memory-pressure heuristics: large external buffers should contribute
/// to GC trigger decisions even though they are not part of the moving heap.
#[no_mangle]
pub extern "C" fn rt_backing_store_external_bytes() -> usize {
  crate::buffer::backing_store::global_backing_store_allocator().external_bytes()
}

// -----------------------------------------------------------------------------
// Global roots / handles (non-stack roots)
// -----------------------------------------------------------------------------

/// Register an addressable root slot with the runtime.
///
/// `slot` must point to a writable `*mut u8` and must remain valid until the
/// returned handle is passed to [`rt_gc_unregister_root_slot`].
#[no_mangle]
pub extern "C" fn rt_gc_register_root_slot(slot: *mut *mut u8) -> u32 {
  crate::roots::global_root_registry().register_root_slot(slot)
}

/// Unregister a previously registered root slot handle.
#[no_mangle]
pub extern "C" fn rt_gc_unregister_root_slot(handle: u32) {
  crate::roots::global_root_registry().unregister(handle);
}

/// Convenience API: create an internal root slot initialized to `ptr`.
///
/// This is primarily intended for FFI/host embeddings that want a persistent
/// handle without managing slot storage themselves.
#[no_mangle]
pub extern "C" fn rt_gc_pin(ptr: *mut u8) -> u32 {
  crate::roots::global_root_registry().pin(ptr)
}

/// Destroy a handle created by [`rt_gc_pin`].
#[no_mangle]
pub extern "C" fn rt_gc_unpin(handle: u32) {
  crate::roots::global_root_registry().unregister(handle);
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_stats_snapshot(out: *mut crate::abi::RtGcStatsSnapshot) {
  if out.is_null() {
    return;
  }
  *out = crate::gc_stats::snapshot();
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub extern "C" fn rt_gc_stats_reset() {
  crate::gc_stats::reset();
}

// -----------------------------------------------------------------------------
// Weak handles (non-owning references)
// -----------------------------------------------------------------------------

/// Create a new weak handle for `value`.
///
/// Weak handles do not keep the referent alive. If the referent is collected, `rt_weak_get`
/// returns null.
#[no_mangle]
pub extern "C" fn rt_weak_add(value: *mut u8) -> u64 {
  crate::gc::weak::global_weak_add(value).as_u64()
}

/// Resolve a weak handle back to a pointer, or null if the referent is dead/cleared.
#[no_mangle]
pub extern "C" fn rt_weak_get(handle: u64) -> *mut u8 {
  crate::gc::weak::global_weak_get(WeakHandle::from_u64(handle)).unwrap_or(std::ptr::null_mut())
}

/// Remove a weak handle.
#[no_mangle]
pub extern "C" fn rt_weak_remove(handle: u64) {
  crate::gc::weak::global_weak_remove(WeakHandle::from_u64(handle));
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.spawn(task, data)
  }));
  match res {
    Ok(id) => id,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(tasks: *const TaskId, count: usize) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.join(tasks, count)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_for(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.parallel_for(start, end, body, data)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

#[no_mangle]
pub extern "C" fn rt_spawn_blocking(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
) -> PromiseRef {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let _ = crate::rt_ensure_init();
    ensure_event_loop_thread_registered();
    crate::blocking_pool::spawn(task, data)
  }));
  match res {
    Ok(p) => p,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_async_spawn_legacy(coro: *mut RtCoroutineHeader) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();
  async_rt::coroutine::async_spawn(coro)
}

/// Drive the runtime's async/event-loop queues.
///
/// This runtime maintains process-global singleton state. `rt_async_poll` may be called from
/// multiple threads, but calls are **globally serialized** (only one thread executes the poll loop
/// at a time).
#[no_mangle]
pub extern "C" fn rt_async_poll_legacy() -> bool {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();
  async_rt::poll()
}

/// Configure whether `await` on an already-settled promise yields to the microtask queue (strict JS
/// semantics) or resumes synchronously (fast-path).
///
/// Default is `false` (fast-path).
#[no_mangle]
pub extern "C" fn rt_async_set_strict_await_yields(strict: bool) {
  async_rt::set_strict_await_yields(strict);
}

#[no_mangle]
pub extern "C" fn rt_async_sleep_legacy(delay_ms: u64) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();

  extern "C" fn resolve_sleep(data: *mut u8) {
    let promise = PromiseRef(data.cast());
    async_rt::promise::promise_resolve(promise, core::ptr::null_mut());
  }

  let promise = async_rt::promise::promise_new();
  let _timer_id = async_rt::global().schedule_timer_in(
    std::time::Duration::from_millis(delay_ms),
    async_rt::Task::new(resolve_sleep, promise.0 as *mut u8),
  );
  promise
}

// -----------------------------------------------------------------------------
// I/O readiness watchers (epoll-backed)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_io_register(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
) -> IoWatcherId {
  let _ = crate::rt_ensure_init();
  let Ok(id) = async_rt::global().register_io(fd, interests, cb, data) else {
    return 0;
  };
  id.as_raw()
}

#[no_mangle]
pub extern "C" fn rt_io_update(id: IoWatcherId, interests: u32) {
  let _ = crate::rt_ensure_init();
  let _ = async_rt::global().update_io(WatcherId::from_raw(id), interests);
}

#[no_mangle]
pub extern "C" fn rt_io_unregister(id: IoWatcherId) {
  let _ = crate::rt_ensure_init();
  let _ = async_rt::global().deregister_fd(WatcherId::from_raw(id));
}

// -----------------------------------------------------------------------------
// Microtasks + timers (queueMicrotask/setTimeout/setInterval)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebTimerKind {
  Timeout,
  Interval,
}

#[derive(Clone, Copy)]
struct WebTimerState {
  kind: WebTimerKind,
  cb: async_rt::TaskFn,
  data: *mut u8,
  interval: Duration,
  internal_id: async_rt::TimerId,
}

// Safety: `WebTimerState` is stored behind a mutex in a process-global map and contains only opaque
// pointers + Copy types. The runtime never dereferences `data`; it is passed back to user callbacks
// on the event-loop thread. Allowing it to cross thread boundaries is therefore safe as far as the
// runtime is concerned (FFI callers are responsible for ensuring their pointers remain valid).
unsafe impl Send for WebTimerState {}

static NEXT_WEB_TIMER_ID: AtomicU64 = AtomicU64::new(1);
static WEB_TIMERS: Lazy<Mutex<HashMap<TimerId, WebTimerState>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn clear_web_timers_for_tests() {
  WEB_TIMERS.lock().clear();
}

fn alloc_web_timer_id() -> TimerId {
  loop {
    let id = NEXT_WEB_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    if id != 0 {
      return id;
    }
  }
}

fn timer_id_to_ptr(id: TimerId) -> *mut u8 {
  id as usize as *mut u8
}

fn timer_id_from_ptr(data: *mut u8) -> TimerId {
  data as usize as TimerId
}

extern "C" fn web_timer_fire(data: *mut u8) {
  let id = timer_id_from_ptr(data);

  let (kind, cb, cb_data, interval) = {
    let mut timers = WEB_TIMERS.lock();
    let Some(st) = timers.get(&id).copied() else {
      return;
    };

    match st.kind {
      WebTimerKind::Timeout => {
        let st = timers.remove(&id).expect("timer entry disappeared");
        (WebTimerKind::Timeout, st.cb, st.data, Duration::ZERO)
      }
      WebTimerKind::Interval => (WebTimerKind::Interval, st.cb, st.data, st.interval),
    }
  };

  (cb)(cb_data);

  if kind != WebTimerKind::Interval {
    return;
  }

  // Reschedule interval if it is still active after the callback.
  let mut timers = WEB_TIMERS.lock();
  let Some(st) = timers.get_mut(&id) else {
    return;
  };
  if st.kind != WebTimerKind::Interval {
    return;
  }

  // HTML clamps nested timers to >= 4ms after a nesting depth of 5. The native runtime does not
  // currently track nesting; higher layers can implement clamping policy if needed.
  let deadline = Instant::now().checked_add(interval).unwrap_or_else(Instant::now);
  let task = async_rt::Task::new(web_timer_fire, data);
  let internal_id = async_rt::global().schedule_timer(deadline, task);
  st.internal_id = internal_id;
}

#[no_mangle]
pub extern "C" fn rt_queue_microtask(cb: extern "C" fn(*mut u8), data: *mut u8) {
  async_rt::enqueue_microtask(cb, data);
}

#[no_mangle]
pub extern "C" fn rt_set_timeout(cb: extern "C" fn(*mut u8), data: *mut u8, delay_ms: u64) -> TimerId {
  let id = alloc_web_timer_id();
  let delay = Duration::from_millis(delay_ms);
  let deadline = Instant::now().checked_add(delay).unwrap_or_else(Instant::now);
  let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
  let internal_id = async_rt::global().schedule_timer(deadline, task);

  WEB_TIMERS.lock().insert(
    id,
    WebTimerState {
      kind: WebTimerKind::Timeout,
      cb,
      data,
      interval: Duration::ZERO,
      internal_id,
    },
  );
  id
}

#[no_mangle]
pub extern "C" fn rt_set_interval(
  cb: extern "C" fn(*mut u8),
  data: *mut u8,
  interval_ms: u64,
) -> TimerId {
  let id = alloc_web_timer_id();
  let interval = Duration::from_millis(interval_ms);
  let deadline = Instant::now().checked_add(interval).unwrap_or_else(Instant::now);
  let task = async_rt::Task::new(web_timer_fire, timer_id_to_ptr(id));
  let internal_id = async_rt::global().schedule_timer(deadline, task);

  WEB_TIMERS.lock().insert(
    id,
    WebTimerState {
      kind: WebTimerKind::Interval,
      cb,
      data,
      interval,
      internal_id,
    },
  );
  id
}

#[no_mangle]
pub extern "C" fn rt_clear_timer(id: TimerId) {
  let Some(st) = WEB_TIMERS.lock().remove(&id) else {
    return;
  };
  let _ = async_rt::global().cancel_timer(st.internal_id);
}

// -----------------------------------------------------------------------------
// Minimal promise ABI (used by async/await lowering)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_promise_new_legacy() -> PromiseRef {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_new()
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve_legacy(p: PromiseRef, value: ValueRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_resolve(p, value)
}

#[no_mangle]
pub extern "C" fn rt_promise_reject_legacy(p: PromiseRef, err: ValueRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_reject(p, err)
}

#[no_mangle]
pub extern "C" fn rt_promise_then_legacy(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_then(p, on_settle, data)
}

#[no_mangle]
pub extern "C" fn rt_coro_await_legacy(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  ensure_event_loop_thread_registered();
  async_rt::coroutine::coro_await(coro, awaited, next_state)
}

// -----------------------------------------------------------------------------
// Thread registration (native codegen / embedding)
// -----------------------------------------------------------------------------

/// Attach the calling OS thread to `runtime`.
///
/// Returns a pointer to the per-thread [`Thread`] record, or null on failure.
///
/// # Safety
/// `runtime` must be a valid pointer to a [`Runtime`] created by the embedder.
#[no_mangle]
pub unsafe extern "C" fn rt_thread_attach(runtime: *mut Runtime) -> *mut Thread {
  let Some(runtime) = runtime.as_ref() else {
    return std::ptr::null_mut();
  };

  match runtime.attach_current_thread_raw() {
    Ok(thread) => thread,
    Err(_) => std::ptr::null_mut(),
  }
}

/// Detach the calling OS thread from its runtime.
///
/// This must be invoked on the *same* OS thread that previously called
/// [`rt_thread_attach`].
///
/// If `thread` is invalid, already detached, or not the current thread, this is
/// a no-op.
///
/// # Safety
/// `thread` must be a pointer previously returned by [`rt_thread_attach`].
#[no_mangle]
pub unsafe extern "C" fn rt_thread_detach(thread: *mut Thread) {
  let Some(thread_ref) = thread.as_ref() else {
    return;
  };

  let runtime = thread_ref.runtime;
  let Some(runtime) = runtime.as_ref() else {
    return;
  };

  // Best-effort: we cannot report errors over this C ABI.
  let _ = runtime.detach_thread_ptr(thread);
}
