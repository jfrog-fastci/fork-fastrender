use runtime_native::abi::Microtask;
use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::shape_table;
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use runtime_native::{rt_async_cancel_all, rt_drain_microtasks, rt_queue_microtask, CoroutineId};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::sync::Once;
static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: mem::size_of::<PromiseHeader>() as u32,
  align: mem::align_of::<PromiseHeader>() as u16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[repr(C)]
struct DropPayload {
  ran: *const AtomicUsize,
  dropped: *const AtomicUsize,
}

extern "C" fn microtask_run(data: *mut u8) {
  // SAFETY: owned by this microtask invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let ran = unsafe { &*payload.ran };
  ran.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn microtask_drop(data: *mut u8) {
  // SAFETY: owned by this microtask drop hook invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let dropped = unsafe { &*payload.dropped };
  dropped.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn noop(_data: *mut u8) {}

#[repr(C)]
struct AwaitCoro {
  header: Coroutine,
  destroyed: *const AtomicUsize,
  await_promise: PromiseRef,
}

unsafe extern "C" fn await_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut AwaitCoro;
  CoroutineStep::await_(unsafe { (*coro).await_promise })
}

unsafe extern "C" fn heap_destroy(coro: CoroutineRef) {
  let coro = coro as *mut AwaitCoro;
  let counter = unsafe { &*(*coro).destroyed };
  counter.fetch_add(1, Ordering::SeqCst);
  unsafe {
    drop(Box::from_raw(coro));
  }
}

static AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_resume,
  destroy: heap_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn cancel_runs_microtask_drop_hook_without_executing() {
  let _rt = TestRuntimeGuard::new();

  let ran = Box::into_raw(Box::new(AtomicUsize::new(0)));
  let dropped = Box::into_raw(Box::new(AtomicUsize::new(0)));

  let payload = Box::new(DropPayload { ran, dropped });
  unsafe {
    rt_queue_microtask(Microtask {
      func: microtask_run,
      data: Box::into_raw(payload).cast(),
      drop: Some(microtask_drop),
    });
  }

  rt_async_cancel_all();

  // The queue should be empty and the microtask must not run.
  assert!(!rt_drain_microtasks());
  assert_eq!(unsafe { &*ran }.load(Ordering::SeqCst), 0);
  assert_eq!(unsafe { &*dropped }.load(Ordering::SeqCst), 1);

  // Idempotent.
  rt_async_cancel_all();

  unsafe {
    drop(Box::from_raw(ran));
    drop(Box::from_raw(dropped));
  }
}

#[test]
fn cancel_drops_pending_native_promise_reactions() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  let destroyed = AtomicUsize::new(0);

  let awaited = Box::new(new_promise_header_pending());
  let awaited_hdr: PromiseRef = Box::into_raw(awaited);

  let mut coro = Box::new(AwaitCoro {
    header: unsafe { core::mem::zeroed() },
    destroyed: &destroyed,
    await_promise: awaited_hdr,
  });
  coro.header.vtable = &AWAIT_VTABLE;
  coro.header.promise = core::ptr::null_mut();
  coro.header.next_waiter = core::ptr::null_mut();
  coro.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  let handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  unsafe {
    let _promise = runtime_native::rt_async_spawn(CoroutineId(handle));
  }

  assert_ne!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "awaiting a pending promise must register a reaction node"
  );

  rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  assert_eq!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "rt_async_cancel_all must detach and drop pending promise reactions"
  );

  unsafe {
    drop(Box::from_raw(awaited_hdr));
  }
}

#[test]
fn cancel_clears_block_on_waker_reactions_when_executor_enters_error_state() {
  let _rt = TestRuntimeGuard::new();

  // Warm up the runtime so this test doesn't include one-time initialization, and to ensure the
  // current thread is registered in the threading registry.
  unsafe {
    let _ = runtime_native::rt_async_run_until_idle_abi();
  }
  let this_thread_id = runtime_native::threading::registry::current_thread_id()
    .expect("rt_async_run_until_idle_abi should register the current thread");

  let awaited = Box::new(new_promise_header_pending());
  let awaited_hdr: PromiseRef = Box::into_raw(awaited);
  let p = runtime_native::PromiseRef(awaited_hdr.cast());
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  runtime_native::rt_async_set_limits(1, 1);

  let (tx, rx) = mpsc::channel::<bool>();
  let error_thread = std::thread::spawn(move || unsafe {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut saw_parked = false;
    while Instant::now() < deadline {
      if runtime_native::threading::all_threads()
        .iter()
        .any(|t| t.id() == this_thread_id && t.is_parked())
      {
        saw_parked = true;
        break;
      }
      std::thread::yield_now();
    }

    // Enqueue two microtasks with a ready queue limit of 1: the second enqueue sets the async
    // executor error state and wakes the event loop.
    runtime_native::rt_queue_microtask(Microtask {
      func: noop,
      data: core::ptr::null_mut(),
      drop: None,
    });
    runtime_native::rt_queue_microtask(Microtask {
      func: noop,
      data: core::ptr::null_mut(),
      drop: None,
    });

    let _ = tx.send(saw_parked);
  });

  // Safety: ABI call.
  unsafe {
    runtime_native::rt_async_block_on(p);
  }

  error_thread.join().unwrap();
  let saw_parked = rx.recv_timeout(Duration::from_secs(1)).unwrap_or(false);
  assert!(saw_parked, "error injector did not observe the event-loop thread parked inside the runtime");

  assert_ne!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "rt_async_block_on should register a waiter reaction before entering the error state"
  );

  rt_async_cancel_all();

  assert_eq!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "rt_async_cancel_all should drop the block_on waker reaction node"
  );

  unsafe {
    drop(Box::from_raw(awaited_hdr));
  }
}
