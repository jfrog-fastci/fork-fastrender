use runtime_native::abi::TaskId;
use runtime_native::{rt_parallel_join, rt_parallel_spawn};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

extern "C" fn inc_counter(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn parallel_join_double_join_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let status = Command::new(exe)
    .arg("--exact")
    .arg("parallel_join_double_join_child")
    .arg("--nocapture")
    .env("RT_PARALLEL_JOIN_DOUBLE_JOIN_CHILD", "1")
    .status()
    .expect("spawn child test process");

  assert!(!status.success(), "expected child to abort");

  // `std::process::abort()` should terminate the process by signal on Unix,
  // rather than returning a normal exit code (e.g. panic exit code 101).
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(6), "expected SIGABRT");
  }
}

#[test]
fn parallel_join_double_join_child() {
  if std::env::var_os("RT_PARALLEL_JOIN_DOUBLE_JOIN_CHILD").is_none() {
    return;
  }

  // Ensure we have a deterministic scheduler configuration.
  std::env::set_var("RT_NUM_THREADS", "2");

  let counter = AtomicUsize::new(0);
  let task = rt_parallel_spawn(inc_counter, (&counter as *const AtomicUsize) as *mut u8);
  rt_parallel_join(&task as *const TaskId, 1);
  assert_eq!(counter.load(Ordering::Relaxed), 1);

  // The `TaskId` contract is "join exactly once". A second join must abort
  // deterministically rather than triggering UB by double-dropping the leaked
  // `Arc<TaskState>`.
  rt_parallel_join(&task as *const TaskId, 1);
}

#[test]
fn parallel_join_misaligned_tasks_pointer_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let status = Command::new(exe)
    .arg("--exact")
    .arg("parallel_join_misaligned_tasks_pointer_child")
    .arg("--nocapture")
    .env("RT_PARALLEL_JOIN_MISALIGNED_PTR_CHILD", "1")
    .status()
    .expect("spawn child test process");

  assert!(!status.success(), "expected child to abort");

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(6), "expected SIGABRT");
  }
}

#[test]
fn parallel_join_misaligned_tasks_pointer_child() {
  if std::env::var_os("RT_PARALLEL_JOIN_MISALIGNED_PTR_CHILD").is_none() {
    return;
  }

  // Join should reject misaligned `TaskId*` pointers before constructing a
  // slice from raw parts. This protects the runtime from UB if the ABI is
  // misused.
  let bytes = vec![0u8; core::mem::size_of::<TaskId>() + 1];
  let ptr = unsafe { bytes.as_ptr().add(1) } as *const TaskId;
  rt_parallel_join(ptr, 1);
}
