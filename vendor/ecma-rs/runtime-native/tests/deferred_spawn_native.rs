use core::ptr::null_mut;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineStepTag, CoroutineVTable, PromiseHeader,
  CORO_FLAG_RUNTIME_OWNS_FRAME, RT_ASYNC_ABI_VERSION,
};
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::CoroutineId;
use runtime_native::PromiseRef as AbiPromiseRef;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  _padding: AtomicUsize,
}

fn abi_promise_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast())
}

unsafe extern "C" fn noop_destroy(_coro: CoroutineRef) {}

#[repr(C)]
struct CounterCoro {
  header: Coroutine,
  counter: *const AtomicUsize,
  promise_ptr: *const AtomicUsize,
}

unsafe extern "C" fn counter_resume(coro: *mut Coroutine) -> CoroutineStep {
  // Safety: CounterCoro is #[repr(C)] and Coroutine is its first field.
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());

  if !(*coro).promise_ptr.is_null() {
    (&*(*coro).promise_ptr).store((*coro).header.promise as usize, Ordering::SeqCst);
  }
  (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
  runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
  CoroutineStep::complete()
}

static COUNTER_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: counter_resume,
  destroy: noop_destroy,
  promise_size: mem::size_of::<TestPromise>() as u32,
  promise_align: mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[repr(C)]
struct YieldOnceCoro {
  header: Coroutine,
  state: u32,
  promise_ptr: *const AtomicUsize,
  started: *mut bool,
  completed: *mut bool,
  awaited: *mut PromiseHeader,
}

unsafe extern "C" fn yield_once_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());

  if !(*coro).promise_ptr.is_null() {
    (&*(*coro).promise_ptr).store((*coro).header.promise as usize, Ordering::SeqCst);
  }

  match (*coro).state {
    0 => {
      *(*coro).started = true;
      (*coro).state = 1;
      CoroutineStep {
        tag: CoroutineStepTag::Await,
        await_promise: (*coro).awaited,
      }
    }
    1 => {
      *(*coro).completed = true;
      runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
      CoroutineStep::complete()
    }
    other => panic!("unexpected coroutine state: {other}"),
  }
}

static YIELD_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: yield_once_resume,
  destroy: noop_destroy,
  promise_size: mem::size_of::<TestPromise>() as u32,
  promise_align: mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

static CORO_HEADER_PTR_OFFSETS: [u32; 2] = [
  mem::offset_of!(Coroutine, promise) as u32,
  mem::offset_of!(Coroutine, next_waiter) as u32,
];
static YIELD_ONCE_CORO_PTR_OFFSETS: [u32; 3] = [
  mem::offset_of!(Coroutine, promise) as u32,
  mem::offset_of!(Coroutine, next_waiter) as u32,
  mem::offset_of!(YieldOnceCoro, awaited) as u32,
];

static SHAPES: [RtShapeDescriptor; 3] = [
  // 1) Promise shape used by the test coroutines' result promises and awaited promises.
  RtShapeDescriptor {
    size: mem::size_of::<TestPromise>() as u32,
    align: mem::align_of::<TestPromise>() as u16,
    flags: 0,
    ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: 0,
    reserved: 0,
  },
  // 2) CounterCoro frame shape.
  RtShapeDescriptor {
    size: mem::size_of::<CounterCoro>() as u32,
    align: mem::align_of::<CounterCoro>() as u16,
    flags: 0,
    ptr_offsets: CORO_HEADER_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: CORO_HEADER_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  // 3) YieldOnceCoro frame shape.
  RtShapeDescriptor {
    size: mem::size_of::<YieldOnceCoro>() as u32,
    align: mem::align_of::<YieldOnceCoro>() as u16,
    flags: 0,
    ptr_offsets: YIELD_ONCE_CORO_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: YIELD_ONCE_CORO_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_obj<T>(shape: RtShapeId) -> *mut T {
  ensure_shape_table();
  runtime_native::rt_alloc(mem::size_of::<T>(), shape).cast::<T>()
}

unsafe fn alloc_promise_pending() -> *mut PromiseHeader {
  let p = alloc_obj::<TestPromise>(RtShapeId(1)).cast::<PromiseHeader>();
  runtime_native::rt_promise_init(abi_promise_from_header(p));
  p
}

#[test]
fn spawn_vs_deferred_spawn_immediacy_native() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  // `rt_async_spawn` resumes the coroutine during the call.
  let counter = AtomicUsize::new(0);
  let promise_ptr = AtomicUsize::new(0);
  let coro = unsafe { alloc_obj::<CounterCoro>(RtShapeId(2)) };
  unsafe {
    // Initialize only the Coroutine fields (avoid overwriting the GC ObjHeader).
    (*coro).header.vtable = &COUNTER_VTABLE;
    (*coro).header.promise = null_mut();
    (*coro).header.next_waiter = null_mut();
    (*coro).header.flags = 0;
    (*coro).counter = &counter;
    (*coro).promise_ptr = &promise_ptr;
  }

  let handle = runtime_native::rt_handle_alloc(coro.cast());
  let promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise.0, unsafe { (*coro).header.promise.cast() });
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // `rt_async_spawn_deferred` only enqueues; no resume until `rt_async_poll`.
  let counter = AtomicUsize::new(0);
  let promise_ptr = AtomicUsize::new(0);
  let coro = unsafe { alloc_obj::<CounterCoro>(RtShapeId(2)) };
  unsafe {
    (*coro).header.vtable = &COUNTER_VTABLE;
    (*coro).header.promise = null_mut();
    (*coro).header.next_waiter = null_mut();
    (*coro).header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
    (*coro).counter = &counter;
    (*coro).promise_ptr = &promise_ptr;
  }

  let handle = runtime_native::rt_handle_alloc(coro.cast());
  let promise = unsafe { runtime_native::rt_async_spawn_deferred(CoroutineId(handle)) };
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), 0);
  assert_eq!(promise.0, unsafe { (*coro).header.promise.cast() });

  while runtime_native::rt_async_poll() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);
  assert!(runtime_native::rt_handle_load(handle).is_null());
}

#[test]
fn deferred_spawn_registers_waiter_when_polled_native() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  let awaited = unsafe { alloc_promise_pending() };

  let promise_ptr = AtomicUsize::new(0);
  let mut started = false;
  let mut completed = false;
  let coro = unsafe { alloc_obj::<YieldOnceCoro>(RtShapeId(3)) };
  unsafe {
    // Initialize only the Coroutine fields (avoid overwriting the GC ObjHeader).
    (*coro).header.vtable = &YIELD_ONCE_VTABLE;
    (*coro).header.promise = null_mut();
    (*coro).header.next_waiter = null_mut();
    (*coro).header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;

    (*coro).state = 0;
    (*coro).promise_ptr = &promise_ptr;
    (*coro).started = &mut started;
    (*coro).completed = &mut completed;
    (*coro).awaited = awaited;
  }

  let handle = runtime_native::rt_handle_alloc(coro.cast());
  let promise = unsafe { runtime_native::rt_async_spawn_deferred(CoroutineId(handle)) };
  assert_eq!(promise.0, unsafe { (*coro).header.promise.cast() });
  assert!(!started);
  assert!(!completed);

  // First poll: coroutine runs and awaits `awaited`, registering a continuation.
  while runtime_native::rt_async_poll() {}
  assert!(started);
  assert!(!completed);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);

  // Settling the awaited promise should enqueue a microtask (not resume immediately).
  unsafe {
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited));
  }
  assert!(!completed);

  while runtime_native::rt_async_poll() {}
  assert!(completed);
  // Coroutine completed after being resumed through a promise reaction; handle must be freed.
  assert!(runtime_native::rt_handle_load(handle).is_null());
}
