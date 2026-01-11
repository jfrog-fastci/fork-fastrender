use core::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::promise_reactions::{PromiseReactionNode, PromiseReactionVTable};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef;
use runtime_native::RtShapeId;

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  _payload: [u8; 8],
}

unsafe fn push_reaction(promise: *mut PromiseHeader, node: *mut PromiseReactionNode) {
  let reactions = &(*promise).waiters;
  loop {
    let head = reactions.load(Ordering::Acquire) as *mut PromiseReactionNode;
    (*node).next = head;
    if reactions
      .compare_exchange(head as usize, node as usize, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      break;
    }
  }
}

#[test]
fn promise_fulfill_drains_reactions_into_microtasks() {
  let _rt = TestRuntimeGuard::new();

  let mut promise = Box::<MaybeUninit<TestPromise>>::new(MaybeUninit::uninit());
  let promise_ptr = promise.as_mut_ptr().cast::<PromiseHeader>();

  unsafe {
    runtime_native::rt_promise_init(PromiseRef(promise_ptr.cast()));
  }

  let fired = AtomicBool::new(false);

  #[repr(C)]
  struct FlagReaction {
    node: PromiseReactionNode,
    fired: *const AtomicBool,
  }

  extern "C" fn flag_run(node: *mut PromiseReactionNode, _promise: runtime_native::async_abi::PromiseRef) {
    let node = unsafe { &*(node as *mut FlagReaction) };
    unsafe { &*node.fired }.store(true, Ordering::Release);
  }

  extern "C" fn flag_drop(node: *mut PromiseReactionNode) {
    unsafe {
      drop(Box::from_raw(node as *mut FlagReaction));
    }
  }

  static FLAG_VTABLE: PromiseReactionVTable = PromiseReactionVTable {
    run: flag_run,
    drop: flag_drop,
  };

  let node = Box::new(FlagReaction {
    node: PromiseReactionNode {
      next: core::ptr::null_mut(),
      vtable: &FLAG_VTABLE,
    },
    fired: &fired,
  });
  let node = Box::into_raw(node).cast::<PromiseReactionNode>();

  unsafe {
    push_reaction(promise_ptr, node);
    runtime_native::rt_promise_fulfill(PromiseRef(promise_ptr.cast()));
  }

  for _ in 0..10 {
    if fired.load(Ordering::Acquire) {
      break;
    }
    let _ = runtime_native::rt_async_poll();
  }

  assert!(fired.load(Ordering::Acquire));
}

#[repr(C)]
struct SpawnCoroutine {
  header: Coroutine,
  state: u32,
  side_effect: *const AtomicBool,
  awaited: *mut PromiseHeader,
}

extern "C" fn spawn_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut SpawnCoroutine;
  unsafe {
    if (*coro).state == 0 {
      (&*(*coro).side_effect).store(true, Ordering::Release);
      (*coro).state = 1;
      return CoroutineStep::await_((*coro).awaited);
    }
  }
  CoroutineStep::complete()
}

unsafe extern "C" fn spawn_destroy(coro: CoroutineRef) {
  unsafe {
    drop(Box::from_raw(coro as *mut SpawnCoroutine));
  }
}

static SPAWN_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: spawn_resume,
  destroy: spawn_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn async_spawn_runs_sync_until_first_await() {
  let _rt = TestRuntimeGuard::new();

  let mut awaited = Box::<MaybeUninit<TestPromise>>::new(MaybeUninit::uninit());
  let awaited_ptr = awaited.as_mut_ptr().cast::<PromiseHeader>();
  unsafe {
    runtime_native::rt_promise_init(PromiseRef(awaited_ptr.cast()));
  }

  let side_effect = AtomicBool::new(false);

  let coro = Box::new(SpawnCoroutine {
    header: Coroutine {
      vtable: &SPAWN_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    side_effect: &side_effect,
    awaited: awaited_ptr,
  });

  let coro_ptr = Box::into_raw(coro).cast::<Coroutine>();
  let handle = runtime_native::rt_handle_alloc(coro_ptr.cast::<u8>());
  let coro_id = runtime_native::CoroutineId(handle);
  let _promise = unsafe { runtime_native::rt_async_spawn(coro_id) };

  assert!(side_effect.load(Ordering::Acquire));

  // Teardown: ensure the coroutine completes and the runtime frees the handle.
  unsafe {
    runtime_native::rt_promise_fulfill(PromiseRef(awaited_ptr.cast()));
  }
  for _ in 0..10 {
    if runtime_native::rt_handle_load(handle).is_null() {
      break;
    }
    let _ = runtime_native::rt_async_poll();
  }
  assert!(runtime_native::rt_handle_load(handle).is_null());
}

#[repr(C)]
struct AwaitCoroutine {
  header: Coroutine,
  state: u32,
  completed: *const AtomicBool,
  awaited: *mut PromiseHeader,
}

extern "C" fn await_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut AwaitCoroutine;
  unsafe {
    match (*coro).state {
      0 => {
        (*coro).state = 1;
        CoroutineStep::await_((*coro).awaited)
      }
      1 => {
        (*coro).state = 2;
        runtime_native::rt_promise_fulfill(PromiseRef((*coro).header.promise.cast()));
        (&*(*coro).completed).store(true, Ordering::Release);
        CoroutineStep::complete()
      }
      _ => CoroutineStep::complete(),
    }
  }
}

unsafe extern "C" fn await_destroy(coro: CoroutineRef) {
  unsafe {
    drop(Box::from_raw(coro as *mut AwaitCoroutine));
  }
}

static AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_resume,
  destroy: await_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn await_reaction_resumes_coroutine_and_completes_promise() {
  let _rt = TestRuntimeGuard::new();

  let mut awaited = Box::<MaybeUninit<TestPromise>>::new(MaybeUninit::uninit());
  let awaited_ptr = awaited.as_mut_ptr().cast::<PromiseHeader>();
  unsafe {
    runtime_native::rt_promise_init(PromiseRef(awaited_ptr.cast()));
  }

  let completed = AtomicBool::new(false);

  let coro = Box::new(AwaitCoroutine {
    header: Coroutine {
      vtable: &AWAIT_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    completed: &completed,
    awaited: awaited_ptr,
  });

  let coro_ptr = Box::into_raw(coro);
  let coro_header = coro_ptr.cast::<Coroutine>();
  let handle = runtime_native::rt_handle_alloc(coro_header.cast::<u8>());
  let promise = unsafe { runtime_native::rt_async_spawn(runtime_native::CoroutineId(handle)) };
  assert!(!completed.load(Ordering::Acquire));
  let coro_promise = unsafe { (*coro_ptr).header.promise };
  assert!(!coro_promise.is_null());
  assert_eq!(
    unsafe { (*coro_promise).state.load(Ordering::Acquire) },
    PromiseHeader::PENDING
  );
  assert_eq!(promise.0.cast::<PromiseHeader>(), coro_promise);

  unsafe {
    runtime_native::rt_promise_fulfill(PromiseRef(awaited_ptr.cast()));
  }
  assert!(!completed.load(Ordering::Acquire));

  for _ in 0..10 {
    if completed.load(Ordering::Acquire) {
      break;
    }
    let _ = runtime_native::rt_async_poll();
  }

  assert!(completed.load(Ordering::Acquire));
  assert!(runtime_native::rt_handle_load(handle).is_null());

  let promise_header = promise.0.cast::<PromiseHeader>();
  assert_eq!(unsafe { (*promise_header).state.load(Ordering::Acquire) }, PromiseHeader::FULFILLED);
}
