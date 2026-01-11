use runtime_native::abi::TaskId;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

#[repr(C)]
struct TaskCtx {
  started: AtomicBool,
  stop: AtomicBool,
}

extern "C" fn spinning_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const TaskCtx) };
  ctx.started.store(true, Ordering::Release);
  while !ctx.stop.load(Ordering::Acquire) {
    runtime_native::rt_gc_safepoint();
    std::hint::spin_loop();
  }
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn stop_the_world_completes_while_thread_waits_in_rt_parallel_join() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let ctx: &'static TaskCtx = Box::leak(Box::new(TaskCtx {
    started: AtomicBool::new(false),
    stop: AtomicBool::new(false),
  }));

  let task_id: TaskId = runtime_native::rt_parallel_spawn(spinning_task, ctx as *const TaskCtx as *mut u8);

  // Wait until a worker has started executing the task so the join thread won't steal it.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !ctx.started.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "parallel task did not start in time");
    std::thread::yield_now();
  }

  let (tx_join_id, rx_join_id) = mpsc::channel();
  let join_thread = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::External);
    tx_join_id.send(id.get()).unwrap();

    runtime_native::rt_parallel_join(&task_id as *const TaskId, 1);

    threading::unregister_current_thread();
  });

  let join_thread_id = rx_join_id.recv().unwrap();

  // Wait until the join thread is blocked in the GC-safe region (NativeSafe).
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let joiner = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == join_thread_id)
      .expect("join thread state");
    if joiner.is_native_safe() {
      break;
    }
    assert!(Instant::now() < deadline, "join thread did not enter NativeSafe in time");
    std::thread::yield_now();
  }

  // Stop-the-world must not wait for a join thread blocked in the runtime.
  runtime_native::rt_gc_request_stop_the_world();
  let _resume = ResumeWorldOnDrop;
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)),
    "world did not reach safepoint in time while join thread was blocked"
  );
  runtime_native::rt_gc_resume_world();

  // Let the task finish and unblock the joiner.
  ctx.stop.store(true, Ordering::Release);
  join_thread.join().unwrap();

  threading::unregister_current_thread();
}

