use runtime_native::abi::RT_IO_READABLE;
use runtime_native::test_util::{reset_runtime_state, TestRuntimeGuard};
use runtime_native::{rt_async_poll_legacy as rt_async_poll, rt_io_register_with_drop, rt_io_unregister};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    if flags < 0 {
      return Err(io::Error::last_os_error());
    }
    if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
      return Err(io::Error::last_os_error());
    }
  }
  Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  set_nonblocking(fds[0])?;
  set_nonblocking(fds[1])?;
  // Safety: `pipe` returns new, owned file descriptors.
  let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((rfd, wfd))
}

#[derive(Default)]
struct Shared {
  id: AtomicU64,
  fired: AtomicBool,
  drop_seen_early: AtomicBool,
  drops: AtomicUsize,
}

#[repr(C)]
struct Holder {
  shared: Arc<Shared>,
}

extern "C" fn noop_io_cb(_events: u32, _data: *mut u8) {}

extern "C" fn drop_holder(data: *mut u8) {
  // Safety: allocated as `Box<Holder>` in the test setup.
  unsafe {
    let holder = Box::from_raw(data as *mut Holder);
    holder.shared.drops.fetch_add(1, Ordering::AcqRel);
  }
}

extern "C" fn on_readable(events: u32, data: *mut u8) {
  if events & RT_IO_READABLE == 0 {
    return;
  }
  // Safety: allocated as `Box<Holder>` in the test setup and freed by `drop_holder`.
  let holder = unsafe { &*(data as *const Holder) };
  holder.shared.fired.store(true, Ordering::Release);

  let id = holder.shared.id.load(Ordering::Acquire);
  rt_io_unregister(id);

  // `drop_holder` must not run while this callback is still executing.
  if holder.shared.drops.load(Ordering::Acquire) != 0 {
    holder.shared.drop_seen_early.store(true, Ordering::Release);
  }
}

#[test]
fn io_register_with_drop_runs_drop_on_unregister() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe().unwrap();

  let shared = Arc::new(Shared::default());
  let holder = Box::new(Holder { shared: shared.clone() });
  let data_ptr = Box::into_raw(holder) as *mut u8;

  let id = rt_io_register_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, noop_io_cb, data_ptr, drop_holder);
  assert_ne!(id, 0);
  shared.id.store(id, Ordering::Release);

  rt_io_unregister(id);

  let start = Instant::now();
  while shared.drops.load(Ordering::Acquire) == 0 {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for watcher drop hook to run"
    );
  }

  assert_eq!(shared.drops.load(Ordering::Acquire), 1);
  assert!(!shared.fired.load(Ordering::Acquire));
}

#[test]
fn io_register_with_drop_does_not_drop_while_callback_running() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe().unwrap();

  let shared = Arc::new(Shared::default());
  let holder = Box::new(Holder { shared: shared.clone() });
  let data_ptr = Box::into_raw(holder) as *mut u8;

  let id = rt_io_register_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, on_readable, data_ptr, drop_holder);
  assert_ne!(id, 0);
  shared.id.store(id, Ordering::Release);

  // Make the pipe readable.
  let buf = [1u8; 1];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
  assert_eq!(rc, 1);

  let start = Instant::now();
  while shared.drops.load(Ordering::Acquire) == 0 {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for readiness callback + drop hook"
    );
  }

  assert!(shared.fired.load(Ordering::Acquire));
  assert!(!shared.drop_seen_early.load(Ordering::Acquire));
  assert_eq!(shared.drops.load(Ordering::Acquire), 1);
}

#[test]
fn io_register_with_drop_runs_drop_on_teardown_clear_watchers() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe().unwrap();

  let shared = Arc::new(Shared::default());
  let holder = Box::new(Holder { shared: shared.clone() });
  let data_ptr = Box::into_raw(holder) as *mut u8;

  let id = rt_io_register_with_drop(rfd.as_raw_fd(), RT_IO_READABLE, noop_io_cb, data_ptr, drop_holder);
  assert_ne!(id, 0);
  shared.id.store(id, Ordering::Release);

  // Simulate teardown: clears watchers and must invoke the drop hook.
  reset_runtime_state();

  assert_eq!(shared.drops.load(Ordering::Acquire), 1);
}

