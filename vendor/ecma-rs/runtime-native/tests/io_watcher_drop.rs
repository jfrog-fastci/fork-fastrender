#![cfg(unix)]

use runtime_native::test_util::TestRuntimeGuard;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};

extern "C" fn noop_cb(_events: u32, _data: *mut u8) {}

extern "C" fn mark_dropped(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

fn set_nonblocking(fd: RawFd) {
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    assert!(flags >= 0, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
    let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    assert!(rc >= 0, "fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
  }
}

fn close(fd: RawFd) {
  unsafe {
    libc::close(fd);
  }
}

#[test]
fn deregister_runs_io_watcher_drop_hook() {
  let _rt = TestRuntimeGuard::new();

  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  let rfd = fds[0];
  let wfd = fds[1];
  set_nonblocking(rfd);
  set_nonblocking(wfd);

  let dropped = Box::new(AtomicBool::new(false));
  let dropped_ptr = Box::into_raw(dropped);

  let id = runtime_native::async_rt::global()
    .register_io_with_drop(rfd, runtime_native::abi::RT_IO_READABLE, noop_cb, dropped_ptr.cast(), mark_dropped)
    .expect("register_io_with_drop failed");

  assert!(runtime_native::async_rt::global().deregister_fd(id));
  assert!(
    unsafe { &*dropped_ptr }.load(Ordering::SeqCst),
    "drop hook must run when the watcher is deregistered"
  );

  unsafe {
    drop(Box::from_raw(dropped_ptr));
  }
  close(rfd);
  close(wfd);
}
