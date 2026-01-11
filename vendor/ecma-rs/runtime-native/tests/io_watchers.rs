use runtime_native::rt_async_poll_legacy as rt_async_poll;
use runtime_native::rt_io_register;
use runtime_native::rt_io_unregister;
use std::sync::mpsc;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux {
  use super::*;
  use runtime_native::abi::RT_IO_READABLE;
  use runtime_native::abi::RT_IO_WRITABLE;
  use libc::c_void;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
  use std::mem;
  use std::os::fd::RawFd;

  static TEST_LOCK: Mutex<()> = Mutex::new(());

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
    let res = unsafe { libc::pipe(fds.as_mut_ptr()) };
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

  #[test]
  fn read_readiness() {
    let _guard = TEST_LOCK.lock().unwrap();
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
    let _guard = TEST_LOCK.lock().unwrap();
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
    let _guard = TEST_LOCK.lock().unwrap();
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
    let _guard = TEST_LOCK.lock().unwrap();
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

  fn pipe() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    let res = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(res, 0);
    (fds[0], fds[1])
  }

  fn close(fd: RawFd) {
    unsafe {
      libc::close(fd);
    }
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
