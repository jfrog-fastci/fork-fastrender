use runtime_native::rt_async_poll_legacy as rt_async_poll;
use runtime_native::rt_io_register;
use runtime_native::rt_io_unregister;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::mpsc;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux {
  use super::*;
  use runtime_native::abi::RT_IO_READABLE;
  use runtime_native::abi::RT_IO_WRITABLE;
  use libc::c_void;
  use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
  use std::mem;
  use std::os::fd::RawFd;

  struct CallbackState {
    fired: AtomicBool,
    events: AtomicU32,
  }

  extern "C" fn record_events(events: u32, data: *mut u8) {
    let state = unsafe { &*(data as *const CallbackState) };
    state.events.store(events, Ordering::SeqCst);
    state.fired.store(true, Ordering::SeqCst);
  }

  extern "C" fn noop_cb(_events: u32, _data: *mut u8) {}

  fn pipe() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    assert_eq!(res, 0);
    (fds[0], fds[1])
  }

  fn close(fd: RawFd) {
    unsafe {
      libc::close(fd);
    }
  }

  fn write_byte(fd: RawFd) {
    let byte: u8 = 1;
    let res = unsafe {
      libc::write(
        fd,
        &byte as *const u8 as *const c_void,
        mem::size_of::<u8>(),
      )
    };
    assert_eq!(res, 1);
  }

  fn set_nonblocking(fd: RawFd) {
    unsafe {
      let flags = libc::fcntl(fd, libc::F_GETFL);
      assert!(flags >= 0, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
      let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
      assert!(rc >= 0, "fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
    }
  }

  #[test]
  fn register_rejects_blocking_fd_without_leaking_registration() {
    let _rt = TestRuntimeGuard::new();

    // Use a blocking pipe to ensure the reactor enforces the edge-triggered/nonblocking contract.
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
    let rfd = fds[0];
    let wfd = fds[1];

    let id = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_eq!(id, 0, "expected blocking fd registration to fail");

    // Ensure the failure didn't leak a registration by setting O_NONBLOCK and re-registering.
    set_nonblocking(rfd);
    let id2 = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(id2, 0, "expected registration to succeed after setting O_NONBLOCK");
    rt_io_unregister(id2);

    close(rfd);
    close(wfd);
  }

  #[test]
  fn read_readiness() {
    let _rt = TestRuntimeGuard::new();
    let (rfd, wfd) = pipe();

    let state = Box::new(CallbackState {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const CallbackState as *mut u8;

    let id = rt_io_register(rfd, RT_IO_READABLE, record_events, state_ptr);
    assert_ne!(id, 0);

    let t = std::thread::spawn(move || {
      write_byte(wfd);
      close(wfd);
    });

    let mut ok = false;
    for _ in 0..16 {
      ok = rt_async_poll();
      if state.fired.load(Ordering::SeqCst) {
        break;
      }
    }
    assert!(ok);
    assert!(state.fired.load(Ordering::SeqCst));
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(events & RT_IO_READABLE, 0);

    rt_io_unregister(id);
    close(rfd);
    t.join().unwrap();
  }

  #[test]
  fn write_readiness() {
    let _rt = TestRuntimeGuard::new();
    let (rfd, wfd) = pipe();

    let state = Box::new(CallbackState {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const CallbackState as *mut u8;

    let id = rt_io_register(wfd, RT_IO_WRITABLE, record_events, state_ptr);
    assert_ne!(id, 0);

    let mut ok = false;
    for _ in 0..16 {
      ok = rt_async_poll();
      if state.fired.load(Ordering::SeqCst) {
        break;
      }
    }
    assert!(ok);
    assert!(state.fired.load(Ordering::SeqCst));
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(events & RT_IO_WRITABLE, 0);

    rt_io_unregister(id);
    close(rfd);
    close(wfd);
  }

  #[test]
  fn unregister_stops_callbacks() {
    let _rt = TestRuntimeGuard::new();
    let (rfd, wfd) = pipe();

    let state = Box::new(CallbackState {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const CallbackState as *mut u8;

    let id = rt_io_register(rfd, RT_IO_READABLE, record_events, state_ptr);
    assert_ne!(id, 0);
    rt_io_unregister(id);

    write_byte(wfd);

    // One poll tick to ensure the runtime processes any pending epoll events.
    let _ = rt_async_poll();
    assert!(!state.fired.load(Ordering::SeqCst));

    close(rfd);
    close(wfd);
  }

  #[test]
  fn thread_safe_register_unregister_wakes_poll() {
    let _rt = TestRuntimeGuard::new();
    // Block the event loop thread in `rt_async_poll` by registering a pipe read end that isn't
    // ready.
    let (block_rfd, block_wfd) = pipe();
    let block_id = rt_io_register(block_rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(block_id, 0);

    let (tx, rx) = mpsc::channel();
    let poll_thread = std::thread::spawn(move || {
      let res = rt_async_poll();
      let _ = tx.send(res);
    });

    // Give the poll thread a moment to enter epoll_wait.
    std::thread::sleep(Duration::from_millis(10));

    let worker = std::thread::spawn(move || {
      // Register and unregister from a non-event-loop thread. This must be thread-safe and must
      // wake the event loop thread.
      let (rfd, wfd) = pipe();
      let id = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
      assert_ne!(id, 0);
      rt_io_unregister(id);
      close(rfd);
      close(wfd);

      // Ensure the polling thread can exit.
      rt_io_unregister(block_id);
    });

    let res = rx.recv_timeout(Duration::from_secs(2)).expect("poll thread did not wake");
    assert!(!res);

    worker.join().unwrap();
    poll_thread.join().unwrap();
    close(block_rfd);
    close(block_wfd);
  }
}

#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]
mod kqueue {
  use super::*;
  use runtime_native::abi::RT_IO_READABLE;
  use std::os::fd::RawFd;
  use std::time::Instant;

  extern "C" fn noop_cb(_events: u32, _data: *mut u8) {}

  fn set_nonblocking(fd: RawFd) {
    unsafe {
      let flags = libc::fcntl(fd, libc::F_GETFL);
      assert!(
        flags >= 0,
        "fcntl(F_GETFL) failed: {}",
        std::io::Error::last_os_error()
      );
      let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
      assert!(
        rc >= 0,
        "fcntl(F_SETFL) failed: {}",
        std::io::Error::last_os_error()
      );
    }
  }

  fn pipe() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    let res = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(res, 0);
    set_nonblocking(fds[0]);
    set_nonblocking(fds[1]);
    (fds[0], fds[1])
  }

  fn close(fd: RawFd) {
    unsafe {
      libc::close(fd);
    }
  }

  #[test]
  fn register_rejects_blocking_fd_without_leaking_registration() {
    let _rt = runtime_native::test_util::TestRuntimeGuard::new();

    // Use a blocking pipe to ensure the reactor enforces the edge-triggered/nonblocking contract.
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
    let rfd = fds[0];
    let wfd = fds[1];

    let id = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_eq!(id, 0, "expected blocking fd registration to fail");

    // Ensure the failure didn't leak a registration by setting O_NONBLOCK and re-registering.
    set_nonblocking(rfd);
    let id2 = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(id2, 0, "expected registration to succeed after setting O_NONBLOCK");
    rt_io_unregister(id2);

    close(rfd);
    close(wfd);
  }

  #[test]
  fn thread_safe_register_unregister_wakes_poll() {
    let _rt = runtime_native::test_util::TestRuntimeGuard::new();

    // Block the event loop thread in `rt_async_poll` by registering a pipe read end that isn't
    // ready.
    let (block_rfd, block_wfd) = pipe();
    let block_id = rt_io_register(block_rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(block_id, 0);

    let (tx, rx) = mpsc::channel();
    let poll_thread = std::thread::spawn(move || {
      let res = rt_async_poll();
      let _ = tx.send(res);
    });

    // Wait until the poll thread is actually blocked in the reactor wait syscall.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
      if runtime_native::async_rt::debug_in_epoll_wait() {
        std::thread::sleep(Duration::from_millis(10));
        if runtime_native::async_rt::debug_in_epoll_wait() {
          break;
        }
      }
      assert!(Instant::now() < deadline, "poll thread did not enter reactor wait syscall");
      std::thread::yield_now();
    }

    let worker = std::thread::spawn(move || {
      // Register and unregister from a non-event-loop thread. This must be thread-safe and must
      // wake the event loop thread.
      let (rfd, wfd) = pipe();
      let id = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
      assert_ne!(id, 0);
      rt_io_unregister(id);
      close(rfd);
      close(wfd);

      // Ensure the polling thread can exit.
      rt_io_unregister(block_id);
    });

    let res = rx.recv_timeout(Duration::from_secs(2)).expect("poll thread did not wake");
    assert!(!res);

    worker.join().unwrap();
    poll_thread.join().unwrap();
    close(block_rfd);
    close(block_wfd);
  }
}

#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
)))]
#[test]
fn io_watchers_not_supported_on_this_platform() {}
