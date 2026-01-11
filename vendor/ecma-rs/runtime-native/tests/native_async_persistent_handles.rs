use core::ptr::null_mut;
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineStepTag, CoroutineVTable, PromiseHeader, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::shape_table;
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use runtime_native::threading;
use runtime_native::CoroutineId;
use runtime_native::PromiseRef as AbiPromiseRef;
use runtime_native::{RtShapeDescriptor, RtShapeId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  _padding: AtomicUsize,
}

impl TestPromise {
  fn new_pending() -> Self {
    Self {
      header: new_promise_header_pending(),
      _padding: AtomicUsize::new(0),
    }
  }
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: core::mem::size_of::<TestPromise>() as u32,
      align: core::mem::align_of::<TestPromise>() as u16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn abi_promise_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast())
}

// -------------------------------------------------------------------------------------------------
// Deferred spawn: microtask task must reload coroutine pointer from persistent-handle slot.
// -------------------------------------------------------------------------------------------------

#[repr(C)]
struct CounterCoro {
  header: Coroutine,
  counter: *const AtomicUsize,
}

unsafe extern "C" fn counter_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());
  (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
  runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
  CoroutineStep::complete()
}

unsafe extern "C" fn counter_destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  // Safety: CounterCoro is #[repr(C)] and Coroutine is its first field. For runtime-owned
  // coroutines, the test passes a `Box::into_raw` pointer to the runtime, and the runtime
  // calls `destroy` exactly once on completion/cancellation.
  unsafe { drop(Box::from_raw(coro as *mut CounterCoro)) };
}

static COUNTER_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: counter_resume,
  destroy: counter_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn deferred_spawn_reloads_coroutine_ptr_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  let c1 = AtomicUsize::new(0);
  let mut coro1 = Box::new(CounterCoro {
    header: unsafe { core::mem::zeroed() },
    counter: &c1,
  });
  coro1.header.vtable = &COUNTER_VTABLE;
  coro1.header.promise = null_mut();
  coro1.header.next_waiter = null_mut();
  coro1.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro1_ref = Box::into_raw(coro1);

  let old_ptr = (coro1_ref as *mut Coroutine).cast::<u8>();
  let handle = runtime_native::rt_handle_alloc(old_ptr);
  // Enqueue the first resume as a microtask.
  let _promise = unsafe { runtime_native::rt_async_spawn_deferred(CoroutineId(handle)) };

  // Allocate an alternate coroutine and point it at the same promise so the microtask can fulfill
  // the promise when it runs.
  let c2 = AtomicUsize::new(0);
  let mut coro2 = Box::new(CounterCoro {
    header: unsafe { core::mem::zeroed() },
    counter: &c2,
  });
  coro2.header.vtable = &COUNTER_VTABLE;
  coro2.header.promise = null_mut();
  coro2.header.next_waiter = null_mut();
  coro2.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro2_ref = Box::into_raw(coro2);
  unsafe {
    (*coro2_ref).header.promise = (*coro1_ref).header.promise;
  }

  let new_ptr = (coro2_ref as *mut Coroutine).cast::<u8>();

  // Simulate a moving GC by updating the persistent-handle slot while the world is stopped.
  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == old_ptr {
        *slot = new_ptr;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(updated, 1, "expected exactly one rooted coroutine pointer slot");

  while runtime_native::rt_async_poll() {}

  assert_eq!(c1.load(Ordering::SeqCst), 0, "original coroutine should not have run after relocation");
  assert_eq!(c2.load(Ordering::SeqCst), 1, "relocated coroutine should have run exactly once");
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // The runtime destroys the relocated coroutine frame (`coro2`). Free the original allocation
  // manually to avoid leaking the test heap.
  unsafe {
    drop(Box::from_raw(coro1_ref));
  }
}

// -------------------------------------------------------------------------------------------------
// Await reaction: promise reaction node must reload coroutine pointer from persistent-handle slot.
// -------------------------------------------------------------------------------------------------

#[repr(C)]
struct YieldOnceCoro {
  header: Coroutine,
  state: u32,
  started: *mut bool,
  completed: *mut bool,
  awaited: *mut PromiseHeader,
}

unsafe extern "C" fn yield_once_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());

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

unsafe extern "C" fn yield_once_destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  // Safety: YieldOnceCoro is #[repr(C)] and Coroutine is its first field. For runtime-owned
  // coroutines, the test passes a `Box::into_raw` pointer to the runtime, and the runtime
  // calls `destroy` exactly once on completion/cancellation.
  unsafe { drop(Box::from_raw(coro as *mut YieldOnceCoro)) };
}

static YIELD_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: yield_once_resume,
  destroy: yield_once_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn await_reaction_reloads_coroutine_ptr_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  let mut awaited = Box::new(TestPromise::new_pending());
  let awaited_ptr: *mut PromiseHeader = &mut awaited.header;
  unsafe {
    runtime_native::rt_promise_init(abi_promise_from_header(awaited_ptr));
  }

  let mut started = false;
  let mut completed = false;
  let mut coro1 = Box::new(YieldOnceCoro {
    header: unsafe { core::mem::zeroed() },
    state: 0,
    started: &mut started,
    completed: &mut completed,
    awaited: awaited_ptr,
  });
  coro1.header.vtable = &YIELD_ONCE_VTABLE;
  coro1.header.promise = null_mut();
  coro1.header.next_waiter = null_mut();
  coro1.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro1_ref = Box::into_raw(coro1);

  // Enqueue and run once so the coroutine registers its await reaction.
  let old_ptr = (coro1_ref as *mut Coroutine).cast::<u8>();
  let handle = runtime_native::rt_handle_alloc(old_ptr);
  let _promise = unsafe { runtime_native::rt_async_spawn_deferred(CoroutineId(handle)) };
  while runtime_native::rt_async_poll() {}
  assert!(started);
  assert!(!completed);

  // Create an alternate coroutine that will be resumed by the await reaction. It starts at state=1
  // (post-await) and points at the same result promise.
  let mut coro2 = Box::new(YieldOnceCoro {
    header: unsafe { core::mem::zeroed() },
    state: 1,
    started: &mut started,
    completed: &mut completed,
    awaited: null_mut(),
  });
  coro2.header.vtable = &YIELD_ONCE_VTABLE;
  coro2.header.promise = unsafe { (*coro1_ref).header.promise };
  coro2.header.next_waiter = null_mut();
  coro2.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;
  let coro2_ref = Box::into_raw(coro2);

  let new_ptr = (coro2_ref as *mut Coroutine).cast::<u8>();

  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == old_ptr {
        *slot = new_ptr;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(updated, 1, "expected exactly one rooted coroutine pointer slot");

  // Settling the awaited promise should enqueue a microtask; the coroutine must resume when we poll.
  unsafe {
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }
  assert!(!completed);

  while runtime_native::rt_async_poll() {}
  assert!(completed);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // The runtime destroys the relocated coroutine frame (`coro2`). Free the original allocation
  // manually to avoid leaking the test heap.
  unsafe {
    drop(Box::from_raw(coro1_ref));
  }
}
