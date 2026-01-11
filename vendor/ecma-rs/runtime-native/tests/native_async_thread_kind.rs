use runtime_native::async_abi::{Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{threading, CoroutineId, RtShapeId};
use std::sync::mpsc;

#[repr(C)]
struct ImmediateCompleteCoro {
  header: Coroutine,
}

extern "C" fn resume_complete(_coro: *mut Coroutine) -> CoroutineStep {
  CoroutineStep::complete()
}

unsafe extern "C" fn destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  unsafe {
    drop(Box::from_raw(coro as *mut ImmediateCompleteCoro));
  }
}

static VTABLE: CoroutineVTable = CoroutineVTable {
  resume: resume_complete,
  destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: runtime_native::async_abi::RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

fn spawn_and_hold(ready: mpsc::Sender<()>, exit: mpsc::Receiver<()>) {
  let coro = Box::new(ImmediateCompleteCoro {
    header: Coroutine {
      vtable: &VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: runtime_native::async_abi::CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
  });

  let handle = runtime_native::rt_handle_alloc(Box::into_raw(coro).cast::<u8>());
  // Safety: `handle` is a valid persistent handle to a coroutine frame whose prefix matches
  // `struct Coroutine`. The runtime consumes the handle and frees it on completion.
  let _promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };

  ready.send(()).unwrap();
  exit.recv().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn rt_async_spawn_does_not_upgrade_non_event_loop_threads_to_main() {
  let _rt = TestRuntimeGuard::new();
  let baseline = threading::thread_counts();

  let (ready_a_tx, ready_a_rx) = mpsc::channel();
  let (exit_a_tx, exit_a_rx) = mpsc::channel();
  let thread_a = std::thread::spawn(move || spawn_and_hold(ready_a_tx, exit_a_rx));
  ready_a_rx.recv().unwrap();

  let (ready_b_tx, ready_b_rx) = mpsc::channel();
  let (exit_b_tx, exit_b_rx) = mpsc::channel();
  let thread_b = std::thread::spawn(move || spawn_and_hold(ready_b_tx, exit_b_rx));
  ready_b_rx.recv().unwrap();

  let counts = threading::thread_counts();
  assert_eq!(
    counts.main,
    baseline.main + 1,
    "exactly one thread should be registered as Main after two rt_async_spawn calls (counts={counts:?}, baseline={baseline:?})"
  );
  assert_eq!(
    counts.external,
    baseline.external + 1,
    "second rt_async_spawn caller should register as External, not Main (counts={counts:?}, baseline={baseline:?})"
  );
  assert_eq!(
    counts.worker, baseline.worker,
    "rt_async_spawn should not spawn/register Worker threads (counts={counts:?}, baseline={baseline:?})"
  );
  assert_eq!(
    counts.io, baseline.io,
    "rt_async_spawn should not spawn/register Io threads (counts={counts:?}, baseline={baseline:?})"
  );
  assert_eq!(
    counts.total,
    baseline.total + 2,
    "expected exactly two additional registered threads (counts={counts:?}, baseline={baseline:?})"
  );

  exit_a_tx.send(()).unwrap();
  exit_b_tx.send(()).unwrap();
  thread_a.join().unwrap();
  thread_b.join().unwrap();
}

