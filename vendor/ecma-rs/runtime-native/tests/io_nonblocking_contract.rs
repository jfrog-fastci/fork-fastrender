#![cfg(target_os = "linux")]

use runtime_native::abi::RT_IO_READABLE;
use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::io::AsyncFd;
use runtime_native::rt_async_poll_legacy as rt_async_poll;
use runtime_native::rt_io_debug;
use runtime_native::rt_io_debug_take_last_error;
use runtime_native::rt_io_register;
use runtime_native::rt_io_register_rooted;
use runtime_native::rt_io_register_with_drop;
use runtime_native::rt_io_unregister;
use runtime_native::rt_io_update;
use runtime_native::test_util::TestRuntimeGuard;
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
