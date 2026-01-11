use runtime_native::abi::TaskId;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::{rt_parallel_join, rt_parallel_spawn};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

extern "C" fn inc_counter(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::Relaxed);
}

struct OuterCtx {
  counter: *const AtomicUsize,
  inner: usize,
  done_tx: mpsc::Sender<()>,
}

extern "C" fn outer_task(data: *mut u8) {
  let ctx = unsafe { Box::from_raw(data as *mut OuterCtx) };

  let mut tasks = Vec::with_capacity(ctx.inner);
  for _ in 0..ctx.inner {
    tasks.push(rt_parallel_spawn(inc_counter, ctx.counter as *mut u8));
  }
  rt_parallel_join(tasks.as_ptr(), tasks.len());

  // Signal the main thread that the nested join completed. The receiver should
  // always be present, but avoid panicking across the `extern "C"` boundary.
  let _ = ctx.done_tx.send(());
}

#[test]
fn nested_join_single_worker_does_not_deadlock() {
  let exe = std::env::current_exe().expect("current_exe");
  let status = Command::new(exe)
    .arg("--exact")
    .arg("nested_join_single_worker_child")
    .arg("--nocapture")
    .env("RT_PARALLEL_SINGLE_WORKER_CHILD", "1")
    // Force the work-stealing pool to a single worker so the nested join relies
    // on joiner-thread task execution (otherwise it would deadlock).
    .env("RT_NUM_THREADS", "1")
    .status()
    .expect("spawn child test process");

  assert!(status.success(), "child process failed: {status}");
}

#[test]
fn nested_join_single_worker_child() {
  if std::env::var_os("RT_PARALLEL_SINGLE_WORKER_CHILD").is_none() {
    return;
  }

  threading::register_current_thread(ThreadKind::Main);

  let counter = AtomicUsize::new(0);
  let (tx, rx) = mpsc::channel();

  let ctx = Box::new(OuterCtx {
    counter: &counter as *const AtomicUsize,
    inner: 1024,
    done_tx: tx,
  });
  let outer_id: TaskId = rt_parallel_spawn(outer_task, Box::into_raw(ctx) as *mut u8);

  // The main thread must not call `rt_parallel_join` here; joiner threads can
  // help execute tasks, which would hide a deadlock if the nested join
  // implementation stopped doing the same on worker threads.
  rx.recv_timeout(Duration::from_secs(5))
    .expect("timed out waiting for nested join to complete");

  // Join the outer task to satisfy the TaskId contract and ensure the runtime
  // cleans up its leaked Arc.
  rt_parallel_join(&outer_id as *const TaskId, 1);
  assert_eq!(counter.load(Ordering::Relaxed), 1024);

  threading::unregister_current_thread();
}

