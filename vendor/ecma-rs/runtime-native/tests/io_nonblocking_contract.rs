#![cfg(target_os = "linux")]

use runtime_native::abi::RT_IO_READABLE;
use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::io::AsyncFd;
use runtime_native::rt_async_poll_legacy as rt_async_poll;
use runtime_native::rt_handle_alloc;
use runtime_native::rt_handle_load;
use runtime_native::rt_io_debug;
use runtime_native::rt_io_debug_take_last_error;
use runtime_native::rt_io_register;
use runtime_native::rt_io_register_handle;
use runtime_native::rt_io_register_handle_with_drop;
use runtime_native::rt_io_register_rooted;
use runtime_native::rt_io_register_with_drop;
use runtime_native::rt_io_unregister;
use runtime_native::rt_io_update;
use runtime_native::test_util::reset_runtime_state;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::GcHeap;
use runtime_native::TypeDescriptor;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

extern "C" fn noop_cb(_events: u32, _data: *mut u8) {}

extern "C" fn set_timeout_flag(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

extern "C" fn inc_drop_count(data: *mut u8) {
  let ctr = unsafe { &*(data as *const AtomicUsize) };
  ctr.fetch_add(1, Ordering::SeqCst);
}

static ROOTED_CB_FIRED: AtomicBool = AtomicBool::new(false);
static ROOTED_CB_PTR: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_rooted_ptr(_events: u32, data: *mut u8) {
  ROOTED_CB_PTR.store(data as usize, Ordering::SeqCst);
  ROOTED_CB_FIRED.store(true, Ordering::SeqCst);
}

fn pipe_blocking() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  // Safety: `pipe` returns new, owned file descriptors.
  let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((rfd, wfd))
}

fn pipe_nonblocking() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  // Safety: `pipe2` returns new, owned file descriptors.
  let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((rfd, wfd))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags != -1 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };
  loop {
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc != -1 {
      break;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  }
  Ok(())
}

fn set_blocking(fd: RawFd) -> io::Result<()> {
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags != -1 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };
  loop {
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if rc != -1 {
      break;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  }
  Ok(())
}

fn write_byte(fd: RawFd) {
  let byte: u8 = 1;
  let rc = unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
  assert_eq!(rc, 1, "write failed: {}", io::Error::last_os_error());
}

const HEADER_SIZE: usize = std::mem::size_of::<ObjHeader>();
static NO_PTR_OFFSETS: [u32; 0] = [];
static ROOTED_OBJ_DESC: TypeDescriptor = TypeDescriptor::new(HEADER_SIZE, &NO_PTR_OFFSETS);

#[repr(C)]
struct HandleDropCounterObj {
  _header: ObjHeader,
  drop_counter: *const AtomicUsize,
}

const HANDLE_DROP_COUNTER_SIZE: usize = std::mem::size_of::<HandleDropCounterObj>();
static HANDLE_DROP_COUNTER_DESC: TypeDescriptor =
  TypeDescriptor::new(HANDLE_DROP_COUNTER_SIZE, &NO_PTR_OFFSETS);

extern "C" fn inc_handle_drop_count(obj: *mut u8) {
  if obj.is_null() {
    return;
  }
  let obj = unsafe { &*(obj as *const HandleDropCounterObj) };
  if obj.drop_counter.is_null() {
    return;
  }
  let ctr = unsafe { &*obj.drop_counter };
  ctr.fetch_add(1, Ordering::SeqCst);
}

fn collect_major(heap: &mut GcHeap) {
  let mut roots = GlobalRootSet::new();
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_major(&mut roots, &mut remembered).unwrap();
}

struct WeakHandleGuard(u64);

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

fn poll_once_with_immediate_timer() -> bool {
  let fired = Box::new(AtomicBool::new(false));
  let fired_ptr: *mut AtomicBool = Box::into_raw(fired);
  runtime_native::async_rt::global().schedule_timer(
    Instant::now(),
    runtime_native::async_rt::Task::new(set_timeout_flag, fired_ptr.cast::<u8>()),
  );

  let pending = rt_async_poll();

  let fired = unsafe { &*fired_ptr };
  assert!(fired.load(Ordering::SeqCst), "timer did not fire");
  unsafe {
    drop(Box::from_raw(fired_ptr));
  }

  pending
}

struct FlagWake {
  flag: Arc<AtomicBool>,
}

impl Wake for FlagWake {
  fn wake(self: Arc<Self>) {
    self.flag.store(true, Ordering::SeqCst);
  }

  fn wake_by_ref(self: &Arc<Self>) {
    self.flag.store(true, Ordering::SeqCst);
  }
}

fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
  Waker::from(Arc::new(FlagWake { flag }))
}

fn block_on_rt<F: Future>(fut: F, timeout: Duration) -> F::Output {
  let timed_out = Box::new(AtomicBool::new(false));
  let timed_out_ptr: *mut AtomicBool = Box::into_raw(timed_out);

  let timer_id = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + timeout,
    runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr.cast::<u8>()),
  );

  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke.clone());
  let mut cx = Context::from_waker(&waker);
  let mut fut = Box::pin(fut);

  loop {
    match fut.as_mut().poll(&mut cx) {
      Poll::Ready(out) => {
        let _ = runtime_native::async_rt::global().cancel_timer(timer_id);
        unsafe {
          drop(Box::from_raw(timed_out_ptr));
        }
        return out;
      }
      Poll::Pending => {
        let _ = rt_async_poll();
        let timed_out = unsafe { &*timed_out_ptr };
        if timed_out.load(Ordering::SeqCst) {
          panic!("timed out waiting for future");
        }
      }
    }
  }
}

#[test]
fn rt_io_register_rejects_blocking_fd_and_reports_error() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_blocking().unwrap();

  let id = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_eq!(id, 0, "expected rt_io_register to fail for blocking fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_FD_NOT_NONBLOCKING,
    "expected rt_io_register failure to be diagnosable as nonblocking contract violation"
  );

  // Ensure the failure didn't leak a watcher (which would keep the runtime non-idle).
  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_invalid_fd_reports_other_error() {
  let _rt = TestRuntimeGuard::new();

  let id = rt_io_register(-1, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_eq!(id, 0, "expected rt_io_register to fail for an invalid fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_OTHER,
    "invalid fd should not be misclassified as a nonblocking contract violation"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_with_drop_calls_drop_on_registration_failure() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_blocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id = rt_io_register_with_drop(
    rfd.as_raw_fd(),
    RT_IO_READABLE,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_eq!(id, 0, "expected rt_io_register_with_drop to fail for blocking fd");
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on registration failure"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_FD_NOT_NONBLOCKING,
    "expected failure to be diagnosable as nonblocking contract violation"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_with_drop_rejects_empty_interests_and_drops_data() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id = rt_io_register_with_drop(
    rfd.as_raw_fd(),
    0,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_eq!(id, 0, "expected rt_io_register_with_drop to fail for empty interests");
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on invalid-interest registration"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected invalid-interest registration to be diagnosable"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_with_drop_invalid_fd_drops_data_and_reports_other_error() {
  let _rt = TestRuntimeGuard::new();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id = rt_io_register_with_drop(
    -1,
    RT_IO_READABLE,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_eq!(id, 0, "expected rt_io_register_with_drop to fail for invalid fd");
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on invalid-fd registration"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_OTHER,
    "invalid fd should not be misclassified as a nonblocking contract violation"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_with_drop_drops_data_on_unregister() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id = rt_io_register_with_drop(
    rfd.as_raw_fd(),
    RT_IO_READABLE,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_ne!(id, 0, "expected rt_io_register_with_drop to succeed");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "successful registration should clear the last error"
  );
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    0,
    "drop_data must not be invoked until unregister"
  );

  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed"
  );

  // Drop hooks may be deferred until a safe point; drive the runtime to ensure it runs.
  runtime_native::rt_async_run_until_idle();
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data must run when the watcher is unregistered"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_with_drop_drops_data_on_reset_runtime_state() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id = rt_io_register_with_drop(
    rfd.as_raw_fd(),
    RT_IO_READABLE,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_ne!(id, 0, "expected rt_io_register_with_drop to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    0,
    "drop_data must not run before teardown"
  );

  // Simulate runtime teardown without explicitly unregistering.
  reset_runtime_state();

  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data must run when the runtime clears watchers during teardown"
  );

  // The watcher id should now be invalid and must not re-run the drop hook.
  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UNREGISTER_FAILED,
    "watcher id should be invalid after runtime teardown"
  );
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data must not be invoked twice"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after teardown");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_with_drop_duplicate_fd_drops_data_and_reports_error() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id1 = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id1, 0, "expected initial registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *mut AtomicUsize = Box::into_raw(dropped);

  let id2 = rt_io_register_with_drop(
    rfd.as_raw_fd(),
    RT_IO_READABLE,
    noop_cb,
    dropped_ptr.cast::<u8>(),
    inc_drop_count,
  );
  assert_eq!(id2, 0, "expected duplicate fd registration to fail");
  assert_eq!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on duplicate registration"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_ALREADY_REGISTERED,
    "expected duplicate registration to be diagnosable"
  );

  rt_io_unregister(id1);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed for the original watcher id"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
}

#[test]
fn rt_io_register_handle_rejects_blocking_fd_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_blocking().unwrap();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, handle);
  assert_eq!(id, 0, "expected rt_io_register_handle to fail for blocking fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_FD_NOT_NONBLOCKING,
    "expected registration failure to be diagnosable as nonblocking contract violation"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_register_handle must free the consumed handle on registration failure"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_handle_rejects_empty_interests_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle(rfd.as_raw_fd(), 0, noop_cb, handle);
  assert_eq!(id, 0, "expected rt_io_register_handle to fail for empty interests");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected invalid-interest registration to be diagnosable"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_register_handle must free the consumed handle on registration failure"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_handle_with_drop_calls_drop_on_registration_failure_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_blocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *const AtomicUsize = &*dropped;

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&HANDLE_DROP_COUNTER_DESC);
  unsafe {
    (*(obj as *mut HandleDropCounterObj)).drop_counter = dropped_ptr;
  }
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, handle, inc_handle_drop_count);
  assert_eq!(
    id, 0,
    "expected rt_io_register_handle_with_drop to fail for blocking fd"
  );
  assert_eq!(
    dropped.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on registration failure"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_FD_NOT_NONBLOCKING,
    "expected registration failure to be diagnosable as nonblocking contract violation"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_register_handle_with_drop must free the consumed handle on registration failure"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_handle_with_drop_rejects_empty_interests_drops_data_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *const AtomicUsize = &*dropped;

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&HANDLE_DROP_COUNTER_DESC);
  unsafe {
    (*(obj as *mut HandleDropCounterObj)).drop_counter = dropped_ptr;
  }
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle_with_drop(rfd.as_raw_fd(), 0, noop_cb, handle, inc_handle_drop_count);
  assert_eq!(
    id, 0,
    "expected rt_io_register_handle_with_drop to fail for empty interests"
  );
  assert_eq!(
    dropped.load(Ordering::SeqCst),
    1,
    "drop_data should have been invoked on invalid-interest registration"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected invalid-interest registration to be diagnosable"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_register_handle_with_drop must free the consumed handle on registration failure"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_handle_rejects_duplicate_fd_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id1 = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id1, 0, "expected initial registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let handle = rt_handle_alloc(obj);

  let id2 = rt_io_register_handle(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, handle);
  assert_eq!(id2, 0, "expected duplicate fd registration to fail");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_ALREADY_REGISTERED,
    "expected duplicate registration to be diagnosable"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_register_handle must free the consumed handle on duplicate registration"
  );

  rt_io_unregister(id1);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed for the original watcher id"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_register_handle_with_drop_drops_data_on_unregister_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *const AtomicUsize = &*dropped;

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&HANDLE_DROP_COUNTER_DESC);
  unsafe {
    (*(obj as *mut HandleDropCounterObj)).drop_counter = dropped_ptr;
  }
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, handle, inc_handle_drop_count);
  assert_ne!(id, 0, "expected rt_io_register_handle_with_drop to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);
  assert_eq!(
    dropped.load(Ordering::SeqCst),
    0,
    "drop_data must not run before unregister"
  );
  assert!(
    !rt_handle_load(handle).is_null(),
    "handle must remain live while the watcher is registered"
  );

  rt_io_unregister(id);
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);
  runtime_native::rt_async_run_until_idle();

  assert_eq!(
    dropped.load(Ordering::SeqCst),
    1,
    "drop_data must run when the watcher is unregistered"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "rt_io_unregister must free the consumed handle"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_register_handle_with_drop_drops_data_on_reset_runtime_state() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let dropped = Box::new(AtomicUsize::new(0));
  let dropped_ptr: *const AtomicUsize = &*dropped;

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&HANDLE_DROP_COUNTER_DESC);
  unsafe {
    (*(obj as *mut HandleDropCounterObj)).drop_counter = dropped_ptr;
  }
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, handle, inc_handle_drop_count);
  assert_ne!(id, 0, "expected rt_io_register_handle_with_drop to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);
  assert_eq!(
    dropped.load(Ordering::SeqCst),
    0,
    "drop_data must not run before teardown"
  );

  // Simulate runtime teardown without explicitly unregistering.
  reset_runtime_state();

  assert_eq!(
    dropped.load(Ordering::SeqCst),
    1,
    "drop_data must run when the runtime clears watchers during teardown"
  );
  assert!(
    rt_handle_load(handle).is_null(),
    "consumed handle must be freed/cleared during teardown"
  );

  // The watcher id should now be invalid and must not re-run the drop hook.
  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UNREGISTER_FAILED,
    "watcher id should be invalid after runtime teardown"
  );
  assert_eq!(
    dropped.load(Ordering::SeqCst),
    1,
    "drop_data must not be invoked twice"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after teardown");
}

#[test]
fn rt_io_register_handle_callback_receives_relocated_ptr_after_gc() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::External);
  let (rfd, wfd) = pipe_nonblocking().unwrap();

  ROOTED_CB_FIRED.store(false, Ordering::SeqCst);
  ROOTED_CB_PTR.store(0, Ordering::SeqCst);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let handle = rt_handle_alloc(obj);

  let id = rt_io_register_handle(rfd.as_raw_fd(), RT_IO_READABLE, record_rooted_ptr, handle);
  assert_ne!(id, 0, "expected handle registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  // Force a major GC to promote/move the object so the watcher must resolve the current pointer
  // value via the persistent handle table.
  collect_major(&mut heap);
  let after_gc = rt_handle_load(handle);
  assert!(!after_gc.is_null());
  assert_ne!(
    after_gc as usize,
    obj as usize,
    "expected major GC to move/promote the object"
  );

  // Ensure `rt_async_poll_legacy` will not block forever if the readiness edge is lost.
  let timed_out = Box::new(AtomicBool::new(false));
  let timed_out_ptr: *mut AtomicBool = Box::into_raw(timed_out);
  let timer_id = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(1),
    runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr.cast::<u8>()),
  );

  write_byte(wfd.as_raw_fd());
  while !ROOTED_CB_FIRED.load(Ordering::SeqCst) {
    let _ = rt_async_poll();
    if unsafe { &*timed_out_ptr }.load(Ordering::SeqCst) {
      panic!("timed out waiting for handle I/O watcher callback");
    }
  }

  let _ = runtime_native::async_rt::global().cancel_timer(timer_id);
  unsafe {
    drop(Box::from_raw(timed_out_ptr));
  }

  assert_eq!(
    ROOTED_CB_PTR.load(Ordering::SeqCst),
    after_gc as usize,
    "handle callback must receive the current relocated pointer value"
  );

  rt_io_unregister(id);
  runtime_native::rt_async_run_until_idle();
  assert!(rt_handle_load(handle).is_null(), "handle must be freed on unregister");

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_register_rooted_failure_does_not_leak_gc_root() {
  let _rt = TestRuntimeGuard::new();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let (rfd, _wfd) = pipe_blocking().unwrap();
  let id = rt_io_register_rooted(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, obj);
  assert_eq!(id, 0, "expected rt_io_register_rooted to fail for blocking fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_FD_NOT_NONBLOCKING,
    "expected failure to be diagnosable as nonblocking contract violation"
  );

  // If the rooted wrapper leaked its GC root on the failure path, the object would remain alive
  // indefinitely. It should become collectable once we run a major collection.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "GC object stayed alive after rooted I/O watcher registration failed (root leak?)"
    );
    std::thread::yield_now();
  }

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_rooted_keeps_gc_object_alive_until_unregistered() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = rt_io_register_rooted(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, obj);
  assert_ne!(id, 0, "expected rooted registration to succeed");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "successful rooted registration should clear the last error"
  );

  // With the watcher still registered, the rooted context must keep the object alive across GC.
  collect_major(&mut heap);
  assert!(
    !runtime_native::rt_weak_get(weak).is_null(),
    "object should remain alive while rooted watcher is registered"
  );

  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed"
  );
  runtime_native::rt_async_run_until_idle();

  // Once unregistered, the rooted context must be released.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted watcher was unregistered (root not released?)"
    );
    std::thread::yield_now();
  }

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_register_rooted_releases_gc_root_on_reset_runtime_state() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = rt_io_register_rooted(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, obj);
  assert_ne!(id, 0, "expected rooted registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  // With the watcher still registered, the rooted context must keep the object alive across GC.
  collect_major(&mut heap);
  assert!(
    !runtime_native::rt_weak_get(weak).is_null(),
    "object should remain alive while rooted watcher is registered"
  );

  // Simulate runtime teardown without explicitly unregistering.
  reset_runtime_state();

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after runtime teardown (root not released?)"
    );
    std::thread::yield_now();
  }

  // The watcher id should now be invalid.
  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UNREGISTER_FAILED,
    "watcher id should be invalid after runtime teardown"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after teardown");
}

#[test]
fn rt_io_register_rooted_callback_receives_relocated_ptr_after_gc() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe_nonblocking().unwrap();

  ROOTED_CB_FIRED.store(false, Ordering::SeqCst);
  ROOTED_CB_PTR.store(0, Ordering::SeqCst);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = rt_io_register_rooted(rfd.as_raw_fd(), RT_IO_READABLE, record_rooted_ptr, obj);
  assert_ne!(id, 0, "expected rooted registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  // Force a major GC to promote/move the object so the rooted watcher must resolve the current
  // pointer value via the persistent handle table.
  collect_major(&mut heap);
  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert_ne!(
    after_gc as usize,
    obj as usize,
    "expected major GC to move/promote the object"
  );

  // Ensure `rt_async_poll_legacy` will not block forever if the readiness edge is lost.
  let timed_out = Box::new(AtomicBool::new(false));
  let timed_out_ptr: *mut AtomicBool = Box::into_raw(timed_out);
  let timer_id = runtime_native::async_rt::global().schedule_timer(
    Instant::now() + Duration::from_secs(1),
    runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr.cast::<u8>()),
  );

  write_byte(wfd.as_raw_fd());
  while !ROOTED_CB_FIRED.load(Ordering::SeqCst) {
    let _ = rt_async_poll();
    if unsafe { &*timed_out_ptr }.load(Ordering::SeqCst) {
      panic!("timed out waiting for rooted I/O watcher callback");
    }
  }

  let _ = runtime_native::async_rt::global().cancel_timer(timer_id);
  unsafe {
    drop(Box::from_raw(timed_out_ptr));
  }

  assert_eq!(
    ROOTED_CB_PTR.load(Ordering::SeqCst),
    after_gc as usize,
    "rooted callback must receive the current relocated pointer value"
  );

  rt_io_unregister(id);
  runtime_native::rt_async_run_until_idle();

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_register_rooted_rejects_empty_interests_and_does_not_leak_root() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = rt_io_register_rooted(rfd.as_raw_fd(), 0, noop_cb, obj);
  assert_eq!(id, 0, "expected rooted registration to fail for empty interests");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected invalid-interest registration to be diagnosable"
  );

  // Since the rooted wrapper is only constructed after interest validation, this should not leak
  // any GC root. The object must be collectable immediately.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "GC object stayed alive after rooted I/O watcher registration failed (root leak?)"
    );
    std::thread::yield_now();
  }

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_rooted_duplicate_fd_does_not_leak_gc_root() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id1 = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id1, 0, "expected initial registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id2 = rt_io_register_rooted(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, obj);
  assert_eq!(id2, 0, "expected rooted registration to fail for duplicate fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_ALREADY_REGISTERED,
    "expected duplicate registration to be diagnosable"
  );

  rt_io_unregister(id1);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed for the original watcher id"
  );

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "GC object stayed alive after rooted I/O watcher registration failed (root leak?)"
    );
    std::thread::yield_now();
  }

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_rooted_invalid_fd_does_not_leak_gc_root() {
  let _rt = TestRuntimeGuard::new();

  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&ROOTED_OBJ_DESC);
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let id = rt_io_register_rooted(-1, RT_IO_READABLE, noop_cb, obj);
  assert_eq!(id, 0, "expected rt_io_register_rooted to fail for invalid fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_OTHER,
    "invalid fd should not be misclassified as a nonblocking contract violation"
  );

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "GC object stayed alive after rooted I/O watcher registration failed (root leak?)"
    );
    std::thread::yield_now();
  }

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_rejects_empty_interests_and_reports_error() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id = rt_io_register(rfd.as_raw_fd(), 0, noop_cb, std::ptr::null_mut());
  assert_eq!(id, 0, "expected rt_io_register to fail for empty interests");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected empty-interest registration to be diagnosable"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle if no watcher leaked");
}

#[test]
fn rt_io_register_rejects_duplicate_fd_and_reports_error() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id1 = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id1, 0, "expected initial registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  let id2 = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_eq!(id2, 0, "expected duplicate fd registration to fail");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_ALREADY_REGISTERED,
    "expected duplicate registration to be diagnosable"
  );

  rt_io_unregister(id1);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed for a valid watcher id"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_update_rejects_empty_interests_and_reports_error() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id, 0, "expected registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  rt_io_update(id, 0);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_INVALID_INTERESTS,
    "expected rt_io_update(0) to be diagnosable"
  );

  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed after a failed update"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_update_invalid_id_reports_error() {
  let _rt = TestRuntimeGuard::new();

  rt_io_update(0, RT_IO_READABLE);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UPDATE_FAILED,
    "expected rt_io_update(0, ...) to report failure for an invalid watcher id"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should remain idle after updating an invalid watcher id");
}

#[test]
fn rt_io_update_detects_nonblocking_contract_violation() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id, 0, "expected registration to succeed");
  assert_eq!(rt_io_debug_take_last_error(), rt_io_debug::OK);

  // Flip the fd back to blocking: the reactor contract requires fds remain O_NONBLOCK.
  set_blocking(rfd.as_raw_fd()).unwrap();

  rt_io_update(id, RT_IO_READABLE);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UPDATE_FAILED,
    "expected rt_io_update to fail for a now-blocking fd"
  );

  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should still succeed after the fd becomes blocking"
  );

  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}

#[test]
fn rt_io_unregister_invalid_id_reports_error() {
  let _rt = TestRuntimeGuard::new();

  rt_io_unregister(0);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UNREGISTER_FAILED,
    "expected rt_io_unregister(0) to be diagnosable"
  );
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_debug_take_last_error should clear the last error"
  );
}

#[test]
fn async_fd_blocking_fd_errors_on_first_poll_and_does_not_leak_registration() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe_blocking().unwrap();
  let afd = AsyncFd::new(rfd);

  // Force an initial registration attempt which must fail for blocking fds.
  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke);
  let mut cx = Context::from_waker(&waker);
  let mut fut = Box::pin(afd.readable());
  match fut.as_mut().poll(&mut cx) {
    Poll::Ready(Err(err)) => {
      assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "got {err:?}");
      assert!(
        err.to_string().contains("O_NONBLOCK"),
        "error message should mention nonblocking contract, got {err:?}"
      );
    }
    other => panic!("expected Poll::Ready(Err(_)), got {other:?}"),
  }
  drop(fut);

  // Ensure the failure didn't leave a stale registration by setting O_NONBLOCK and re-awaiting.
  set_nonblocking(afd.as_raw_fd()).unwrap();

  let writer = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(10));
    write_byte(wfd.as_raw_fd());
  });

  block_on_rt(async { afd.readable().await.unwrap() }, Duration::from_secs(1));
  writer.join().unwrap();
}

#[test]
fn rt_io_update_closed_fd_fails_gracefully_and_can_be_unregistered() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe_nonblocking().unwrap();

  let id = rt_io_register(rfd.as_raw_fd(), RT_IO_READABLE, noop_cb, std::ptr::null_mut());
  assert_ne!(id, 0, "expected rt_io_register to succeed for nonblocking fd");
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "successful rt_io_register should clear the last error"
  );

  // Closing the fd should cause subsequent update attempts to fail gracefully.
  drop(rfd);

  rt_io_update(id, RT_IO_READABLE);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::ERR_UPDATE_FAILED,
    "expected rt_io_update to report failure for a closed fd"
  );

  rt_io_unregister(id);
  assert_eq!(
    rt_io_debug_take_last_error(),
    rt_io_debug::OK,
    "rt_io_unregister should succeed even if the underlying fd has already been closed"
  );

  // Ensure the watcher bookkeeping was removed (avoid leaving the runtime non-idle).
  let pending = poll_once_with_immediate_timer();
  assert!(!pending, "runtime should be idle after unregistering the watcher");
}
