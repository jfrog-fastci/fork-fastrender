use runtime_native::rt_async_poll_legacy as rt_async_poll;
use runtime_native::rt_io_register;
use runtime_native::rt_io_update;
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
  use std::time::Instant;
  use std::io;

  extern "C" fn set_timeout_flag(data: *mut u8) {
    let flag = unsafe { &*(data as *const AtomicBool) };
    flag.store(true, Ordering::SeqCst);
  }

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

  fn socketpair() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "socketpair failed: {}", io::Error::last_os_error());
    set_nonblocking(fds[0]);
    set_nonblocking(fds[1]);
    (fds[0], fds[1])
  }

  fn fill_write_buffer(fd: RawFd) {
    let buf = [0u8; 4096];
    loop {
      let rc = unsafe { libc::write(fd, buf.as_ptr().cast::<c_void>(), buf.len()) };
      if rc > 0 {
        continue;
      }
      if rc < 0 {
        let err = io::Error::last_os_error();
        match err.kind() {
          io::ErrorKind::Interrupted => continue,
          io::ErrorKind::WouldBlock => break,
          _ => panic!("fill_write_buffer: write failed: {err}"),
        }
      }
      break;
    }
  }

  fn drain_read(fd: RawFd) {
    let mut buf = [0u8; 4096];
    loop {
      let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
      if rc > 0 {
        continue;
      }
      if rc == 0 {
        break;
      }
      let err = io::Error::last_os_error();
      match err.kind() {
        io::ErrorKind::Interrupted => continue,
        io::ErrorKind::WouldBlock => break,
        _ => panic!("drain_read: read failed: {err}"),
      }
    }
  }

  #[test]
  fn register_rejects_empty_interests_without_leaking_pending_work() {
    let _rt = TestRuntimeGuard::new();
    let (rfd, wfd) = pipe();
    assert_eq!(
      rt_io_register(rfd, 0, noop_cb, std::ptr::null_mut()).0,
      0,
      "expected empty-interest registration to fail"
    );
    close(rfd);
    close(wfd);

    let fired = Box::new(AtomicBool::new(false));
    let fired_ptr: *mut AtomicBool = Box::into_raw(fired);
    runtime_native::async_rt::global().schedule_timer(
      std::time::Instant::now(),
      runtime_native::async_rt::Task::new(set_timeout_flag, fired_ptr.cast::<u8>()),
    );

    let pending = rt_async_poll();
    let fired = unsafe { &*fired_ptr };
    assert!(fired.load(Ordering::SeqCst), "timer did not fire");
    unsafe {
      drop(Box::from_raw(fired_ptr));
    }
    assert!(
      !pending,
      "async runtime should be idle after the timer if no watcher leaked"
    );
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
    assert_eq!(id.0, 0, "expected blocking fd registration to fail");

    // Ensure the failure didn't leak a registration by setting O_NONBLOCK and re-registering.
    set_nonblocking(rfd);
    let id2 = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(id2.0, 0, "expected registration to succeed after setting O_NONBLOCK");
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
    assert_ne!(id.0, 0);

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
    assert_ne!(id.0, 0);

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
    assert_ne!(id.0, 0);
    rt_io_unregister(id);

    write_byte(wfd);

    // One poll tick to ensure the runtime processes any pending epoll events.
    let _ = rt_async_poll();
    assert!(!state.fired.load(Ordering::SeqCst));

    close(rfd);
    close(wfd);
  }

  #[test]
  fn update_interest_to_writable() {
    let _rt = TestRuntimeGuard::new();

    let (a, b) = socketpair();

    // Fill `a`'s send buffer so it transitions from not-writable -> writable once we drain `b`.
    fill_write_buffer(a);

    struct State {
      fired: AtomicBool,
      events: AtomicU32,
    }

    extern "C" fn record_events(events: u32, data: *mut u8) {
      let state = unsafe { &*(data as *const State) };
      state.events.fetch_or(events, Ordering::SeqCst);
      state.fired.store(true, Ordering::SeqCst);
    }

    let state = Box::new(State {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const State as *mut u8;

    // Start by registering READABLE only, then update to WRITABLE.
    let id = rt_io_register(a, RT_IO_READABLE, record_events, state_ptr);
    assert_ne!(id.0, 0);
    rt_io_update(id, RT_IO_WRITABLE);

    // Drain `b` to free space in `a`'s send buffer, producing a WRITABLE edge on `a`.
    drain_read(b);

    // Fail-safe timer so the test can't hang if the writable edge is missed.
    let timed_out = Box::new(AtomicBool::new(false));
    let timed_out_ptr = timed_out.as_ref() as *const AtomicBool as *mut u8;
    let timer_id = runtime_native::async_rt::global().schedule_timer(
      Instant::now() + Duration::from_secs(1),
      runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr),
    );

    while !state.fired.load(Ordering::SeqCst) && !timed_out.load(Ordering::SeqCst) {
      rt_async_poll();
    }

    let _ = runtime_native::async_rt::global().cancel_timer(timer_id);

    assert!(!timed_out.load(Ordering::SeqCst), "timed out waiting for writable readiness");
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(
      events & RT_IO_WRITABLE,
      0,
      "expected RT_IO_WRITABLE after rt_io_update, got events=0x{events:x}"
    );

    rt_io_unregister(id);
    close(a);
    close(b);
  }

  #[test]
  fn write_readiness_after_register_socketpair() {
    let _rt = TestRuntimeGuard::new();

    let (a, b) = socketpair();

    // Fill `a`'s send buffer so it transitions from not-writable -> writable once we drain `b`.
    fill_write_buffer(a);

    struct State {
      fired: AtomicBool,
      events: AtomicU32,
    }

    extern "C" fn record_events(events: u32, data: *mut u8) {
      let state = unsafe { &*(data as *const State) };
      state.events.fetch_or(events, Ordering::SeqCst);
      state.fired.store(true, Ordering::SeqCst);
    }

    let state = Box::new(State {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const State as *mut u8;

    let id = rt_io_register(a, RT_IO_WRITABLE, record_events, state_ptr);
    assert_ne!(id.0, 0);

    // Drain `b` to free space in `a`'s send buffer, producing a WRITABLE edge on `a`.
    drain_read(b);

    // Fail-safe timer so the test can't hang if the writable edge is missed.
    let timed_out = Box::new(AtomicBool::new(false));
    let timed_out_ptr = timed_out.as_ref() as *const AtomicBool as *mut u8;
    let timer_id = runtime_native::async_rt::global().schedule_timer(
      Instant::now() + Duration::from_secs(1),
      runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr),
    );

    while !state.fired.load(Ordering::SeqCst) && !timed_out.load(Ordering::SeqCst) {
      rt_async_poll();
    }

    let _ = runtime_native::async_rt::global().cancel_timer(timer_id);

    assert!(
      !timed_out.load(Ordering::SeqCst),
      "timed out waiting for writable readiness"
    );
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(
      events & RT_IO_WRITABLE,
      0,
      "expected RT_IO_WRITABLE, got events=0x{events:x}"
    );

    rt_io_unregister(id);
    close(a);
    close(b);
  }

  #[test]
  fn thread_safe_register_unregister_wakes_poll() {
    let _rt = TestRuntimeGuard::new();
    // Block the event loop thread in `rt_async_poll` by registering a pipe read end that isn't
    // ready.
    let (block_rfd, block_wfd) = pipe();
    let block_id = rt_io_register(block_rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(block_id.0, 0);

    let (tx, rx) = mpsc::channel();
    let poll_thread = std::thread::spawn(move || {
      let res = rt_async_poll();
      let _ = tx.send(res);
    });

    // Wait until the poll thread is actually blocked in the reactor wait syscall.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
      if runtime_native::async_rt::debug_in_epoll_wait() {
        // Ensure it's not just a transient enter/exit.
        std::thread::sleep(Duration::from_millis(10));
        if runtime_native::async_rt::debug_in_epoll_wait() {
          break;
        }
      }
      assert!(
        Instant::now() < deadline,
        "poll thread did not enter reactor wait syscall"
      );
      std::thread::yield_now();
    }

    let worker = std::thread::spawn(move || {
      // Register and unregister from a non-event-loop thread. This must be thread-safe and must
      // wake the event loop thread.
      let (rfd, wfd) = pipe();
      let id = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
      assert_ne!(id.0, 0);
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
  use runtime_native::abi::RT_IO_WRITABLE;
  use std::os::fd::RawFd;
  use std::time::Instant;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::atomic::AtomicU32;
  use std::io;

  extern "C" fn noop_cb(_events: u32, _data: *mut u8) {}
  extern "C" fn set_timeout_flag(data: *mut u8) {
    let flag = unsafe { &*(data as *const AtomicBool) };
    flag.store(true, Ordering::SeqCst);
  }

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

  fn socketpair() -> (RawFd, RawFd) {
    let mut fds = [0; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "socketpair failed: {}", io::Error::last_os_error());
    set_nonblocking(fds[0]);
    set_nonblocking(fds[1]);
    (fds[0], fds[1])
  }

  fn close(fd: RawFd) {
    unsafe {
      libc::close(fd);
    }
  }

  fn fill_write_buffer(fd: RawFd) {
    let buf = [0u8; 4096];
    loop {
      let rc = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
      if rc > 0 {
        continue;
      }
      if rc < 0 {
        let err = io::Error::last_os_error();
        match err.kind() {
          io::ErrorKind::Interrupted => continue,
          io::ErrorKind::WouldBlock => break,
          _ => panic!("fill_write_buffer: write failed: {err}"),
        }
      }
      break;
    }
  }

  fn drain_read(fd: RawFd) {
    let mut buf = [0u8; 4096];
    loop {
      let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
      if rc > 0 {
        continue;
      }
      if rc == 0 {
        break;
      }
      let err = io::Error::last_os_error();
      match err.kind() {
        io::ErrorKind::Interrupted => continue,
        io::ErrorKind::WouldBlock => break,
        _ => panic!("drain_read: read failed: {err}"),
      }
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
    assert_eq!(id.0, 0, "expected blocking fd registration to fail");

    // Ensure the failure didn't leak a registration by setting O_NONBLOCK and re-registering.
    set_nonblocking(rfd);
    let id2 = rt_io_register(rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(id2.0, 0, "expected registration to succeed after setting O_NONBLOCK");
    rt_io_unregister(id2);

    close(rfd);
    close(wfd);
  }

  #[test]
  fn register_rejects_empty_interests_without_leaking_pending_work() {
    let _rt = TestRuntimeGuard::new();
    let (rfd, wfd) = pipe();
    assert_eq!(
      rt_io_register(rfd, 0, noop_cb, std::ptr::null_mut()).0,
      0,
      "expected empty-interest registration to fail"
    );
    close(rfd);
    close(wfd);

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
    assert!(
      !pending,
      "async runtime should be idle after the timer if no watcher leaked"
    );
  }

  #[test]
  fn thread_safe_register_unregister_wakes_poll() {
    let _rt = TestRuntimeGuard::new();

    // Block the event loop thread in `rt_async_poll` by registering a pipe read end that isn't
    // ready.
    let (block_rfd, block_wfd) = pipe();
    let block_id = rt_io_register(block_rfd, RT_IO_READABLE, noop_cb, std::ptr::null_mut());
    assert_ne!(block_id.0, 0);

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
      assert_ne!(id.0, 0);
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

  #[test]
  fn update_interest_to_writable() {
    let _rt = runtime_native::test_util::TestRuntimeGuard::new();

    let (a, b) = socketpair();

    // Fill `a`'s send buffer so it transitions from not-writable -> writable once we drain `b`.
    fill_write_buffer(a);

    struct State {
      fired: AtomicBool,
      events: AtomicU32,
    }

    extern "C" fn record_events(events: u32, data: *mut u8) {
      let state = unsafe { &*(data as *const State) };
      state.events.fetch_or(events, Ordering::SeqCst);
      state.fired.store(true, Ordering::SeqCst);
    }

    let state = Box::new(State {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const State as *mut u8;

    // Start by registering READABLE only, then update to WRITABLE.
    let id = rt_io_register(a, RT_IO_READABLE, record_events, state_ptr);
    assert_ne!(id.0, 0);
    rt_io_update(id, RT_IO_WRITABLE);

    // Drain `b` to free space in `a`'s send buffer, producing a WRITABLE edge on `a`.
    drain_read(b);

    // Fail-safe timer so the test can't hang if the writable edge is missed.
    let timed_out = Box::new(AtomicBool::new(false));
    let timed_out_ptr = timed_out.as_ref() as *const AtomicBool as *mut u8;
    let timer_id = runtime_native::async_rt::global().schedule_timer(
      Instant::now() + Duration::from_secs(1),
      runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr),
    );

    while !state.fired.load(Ordering::SeqCst) && !timed_out.load(Ordering::SeqCst) {
      rt_async_poll();
    }

    let _ = runtime_native::async_rt::global().cancel_timer(timer_id);

    assert!(
      !timed_out.load(Ordering::SeqCst),
      "timed out waiting for writable readiness"
    );
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(
      events & RT_IO_WRITABLE,
      0,
      "expected RT_IO_WRITABLE after rt_io_update, got events=0x{events:x}"
    );

    rt_io_unregister(id);
    close(a);
    close(b);
  }

  #[test]
  fn write_readiness_after_register_socketpair() {
    let _rt = runtime_native::test_util::TestRuntimeGuard::new();

    let (a, b) = socketpair();

    // Fill `a`'s send buffer so it transitions from not-writable -> writable once we drain `b`.
    fill_write_buffer(a);

    struct State {
      fired: AtomicBool,
      events: AtomicU32,
    }

    extern "C" fn record_events(events: u32, data: *mut u8) {
      let state = unsafe { &*(data as *const State) };
      state.events.fetch_or(events, Ordering::SeqCst);
      state.fired.store(true, Ordering::SeqCst);
    }

    let state = Box::new(State {
      fired: AtomicBool::new(false),
      events: AtomicU32::new(0),
    });
    let state_ptr = state.as_ref() as *const State as *mut u8;

    let id = rt_io_register(a, RT_IO_WRITABLE, record_events, state_ptr);
    assert_ne!(id.0, 0);

    // Drain `b` to free space in `a`'s send buffer, producing a WRITABLE edge on `a`.
    drain_read(b);

    // Fail-safe timer so the test can't hang if the writable edge is missed.
    let timed_out = Box::new(AtomicBool::new(false));
    let timed_out_ptr = timed_out.as_ref() as *const AtomicBool as *mut u8;
    let timer_id = runtime_native::async_rt::global().schedule_timer(
      Instant::now() + Duration::from_secs(1),
      runtime_native::async_rt::Task::new(set_timeout_flag, timed_out_ptr),
    );

    while !state.fired.load(Ordering::SeqCst) && !timed_out.load(Ordering::SeqCst) {
      rt_async_poll();
    }

    let _ = runtime_native::async_rt::global().cancel_timer(timer_id);

    assert!(
      !timed_out.load(Ordering::SeqCst),
      "timed out waiting for writable readiness"
    );
    let events = state.events.load(Ordering::SeqCst);
    assert_ne!(
      events & RT_IO_WRITABLE,
      0,
      "expected RT_IO_WRITABLE, got events=0x{events:x}"
    );

    rt_io_unregister(id);
    close(a);
    close(b);
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
fn io_watchers_not_supported_on_this_platform() {
  let _rt = TestRuntimeGuard::new();
}
