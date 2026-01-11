#![cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::abi::RT_IO_READABLE;
use runtime_native::async_rt;
use runtime_native::rt_io_register;
use runtime_native::rt_io_unregister;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

extern "C" fn inc_counter(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

fn make_pipe() -> (i32, i32) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
  (fds[0], fds[1])
}

fn close_fd(fd: i32) {
  unsafe {
    let _ = libc::close(fd);
  }
}

fn write_byte(fd: i32) {
  let byte: [u8; 1] = [1];
  let rc = unsafe { libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), byte.len()) };
  assert_eq!(rc, 1, "write failed: {}", std::io::Error::last_os_error());
}

#[test]
fn read_ready_pipe() {
  let _rt = TestRuntimeGuard::new();

  let (rfd, wfd) = make_pipe();

  let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let fired_ptr = fired as *const AtomicBool as *mut u8;

  extern "C" fn on_events(events: u32, data: *mut u8) {
    let fired = unsafe { &*(data as *const AtomicBool) };
    if events & RT_IO_READABLE != 0 {
      fired.store(true, Ordering::SeqCst);
    }
  }

  let id = rt_io_register(rfd, RT_IO_READABLE, on_events, fired_ptr);
  assert_ne!(id, 0);

  let writer = std::thread::spawn(move || {
    write_byte(wfd);
    close_fd(wfd);
  });

  let deadline = Instant::now() + Duration::from_secs(1);
  while !fired.load(Ordering::SeqCst) {
    assert!(Instant::now() < deadline, "timed out waiting for readability");
    runtime_native::rt_async_poll_legacy();
  }

  rt_io_unregister(id);
  close_fd(rfd);
  writer.join().unwrap();
}

#[test]
fn deregister_stops_events() {
  let _rt = TestRuntimeGuard::new();

  let (rfd, wfd) = make_pipe();

  let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let fired_ptr = fired as *const AtomicBool as *mut u8;

  extern "C" fn on_events(_events: u32, data: *mut u8) {
    let fired = unsafe { &*(data as *const AtomicBool) };
    fired.store(true, Ordering::SeqCst);
  }

  let id = rt_io_register(rfd, RT_IO_READABLE, on_events, fired_ptr);
  assert_ne!(id, 0);
  rt_io_unregister(id);

  write_byte(wfd);
  runtime_native::rt_async_poll_legacy();

  assert!(!fired.load(Ordering::SeqCst), "callback fired after deregister");

  close_fd(rfd);
  close_fd(wfd);
}

#[test]
fn wake_interrupts_poll() {
  let _rt = TestRuntimeGuard::new();

  let ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let timer_fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  // Keep the runtime non-idle so `rt_async_poll` blocks inside the reactor.
  let timer_id = async_rt::schedule_timer(
    Instant::now() + Duration::from_secs(1),
    set_bool,
    timer_fired as *const AtomicBool as *mut u8,
  );

  let (tx, rx) = mpsc::channel();
  let poll_thread = std::thread::spawn(move || {
    let start = Instant::now();
    let _pending = runtime_native::rt_async_poll_legacy();
    let _ = tx.send(start.elapsed());
  });

  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    if async_rt::debug_in_epoll_wait() {
      break;
    }
    assert!(Instant::now() < deadline, "poll thread did not block in time");
    std::thread::yield_now();
  }

  async_rt::enqueue_microtask(set_bool, ran as *const AtomicBool as *mut u8);

  let elapsed = rx
    .recv_timeout(Duration::from_secs(2))
    .expect("poll thread did not return in time");

  let _ = async_rt::global().cancel_timer(timer_id);
  poll_thread.join().unwrap();

  assert!(ran.load(Ordering::SeqCst), "microtask did not run");
  assert!(
    elapsed < Duration::from_millis(500),
    "rt_async_poll did not wake promptly (elapsed={elapsed:?})"
  );
  assert!(!timer_fired.load(Ordering::SeqCst), "poll returned only after timer fired");
}

#[test]
fn wake_race_stress() {
  let _rt = TestRuntimeGuard::new();

  let ran: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
  let timer_fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  // Keep the runtime non-idle so `rt_async_poll` blocks inside the reactor.
  let timer_id = async_rt::schedule_timer(
    Instant::now() + Duration::from_secs(2),
    set_bool,
    timer_fired as *const AtomicBool as *mut u8,
  );

  let (tx, rx) = mpsc::channel();
  let poll_thread = std::thread::spawn(move || {
    let start = Instant::now();
    let _pending = runtime_native::rt_async_poll_legacy();
    let _ = tx.send(start.elapsed());
  });

  // Wait for the polling thread to actually block in `kevent`.
  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    if async_rt::debug_in_epoll_wait() {
      std::thread::sleep(Duration::from_millis(10));
      if async_rt::debug_in_epoll_wait() {
        break;
      }
    }
    assert!(Instant::now() < deadline, "poll thread did not enter kevent in time");
    std::thread::yield_now();
  }

  let mut wakers = Vec::new();
  for _ in 0..4 {
    let ran_ptr = ran as *const AtomicUsize as *mut u8;
    wakers.push(std::thread::spawn(move || {
      for _ in 0..200 {
        async_rt::enqueue_microtask(inc_counter, ran_ptr);
      }
    }));
  }
  for w in wakers {
    w.join().unwrap();
  }

  let elapsed = rx
    .recv_timeout(Duration::from_secs(3))
    .expect("poll thread did not return in time");

  let _ = async_rt::global().cancel_timer(timer_id);
  poll_thread.join().unwrap();

  assert!(ran.load(Ordering::SeqCst) > 0, "microtasks did not run");
  assert!(
    elapsed < Duration::from_millis(500),
    "rt_async_poll did not wake promptly (elapsed={elapsed:?})"
  );
  assert!(!timer_fired.load(Ordering::SeqCst), "poll returned only after timer fired");
}
