use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::task::RawWaker;
use std::task::RawWakerVTable;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use runtime_native::reactor::Interest;
use runtime_native::ReactorDriver;

fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
  unsafe fn clone(data: *const ()) -> RawWaker {
    let arc = Arc::<AtomicUsize>::from_raw(data.cast());
    let cloned = arc.clone();
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
  }

  unsafe fn wake(data: *const ()) {
    let arc = Arc::<AtomicUsize>::from_raw(data.cast());
    arc.fetch_add(1, Ordering::SeqCst);
    // drop
  }

  unsafe fn wake_by_ref(data: *const ()) {
    let arc = Arc::<AtomicUsize>::from_raw(data.cast());
    arc.fetch_add(1, Ordering::SeqCst);
    std::mem::forget(arc);
  }

  unsafe fn drop(data: *const ()) {
    let _ = Arc::<AtomicUsize>::from_raw(data.cast());
  }

  static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

  unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE)) }
}

#[test]
fn timer_wakes_exactly_once() {
  let driver = ReactorDriver::new().unwrap();

  let fired = Arc::new(AtomicUsize::new(0));
  driver.register_timer(Instant::now() + Duration::from_millis(30), counting_waker(fired.clone()));

  // Use a generous upper bound so failures don't hang indefinitely.
  let out = driver.poll(Some(Duration::from_secs(1))).unwrap();
  assert_eq!(out.timers_fired, 1);
  assert_eq!(fired.load(Ordering::SeqCst), 1);

  // Poll again; the timer should not fire twice.
  let out = driver.poll(Some(Duration::ZERO)).unwrap();
  assert_eq!(out.timers_fired, 0);
  assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[test]
fn timer_beats_large_poll_timeout_even_with_registered_fd() {
  let driver = ReactorDriver::new().unwrap();

  let (read_fd, write_fd) = new_pipe().unwrap();
  set_nonblocking(read_fd.as_raw_fd()).unwrap();
  let io_wakes = Arc::new(AtomicUsize::new(0));
  driver
    .register_fd(read_fd.as_fd(), Interest::READABLE, counting_waker(io_wakes.clone()))
    .unwrap();

  let timer_wakes = Arc::new(AtomicUsize::new(0));
  driver.register_timer(Instant::now() + Duration::from_millis(50), counting_waker(timer_wakes.clone()));

  let start = Instant::now();
  let out = driver.poll(Some(Duration::from_secs(2))).unwrap();
  let elapsed = start.elapsed();

  // The poll should return due to the timer, not by sleeping the full timeout.
  assert!(elapsed < Duration::from_secs(1), "poll slept too long: {elapsed:?}");
  assert_eq!(out.timers_fired, 1);
  assert_eq!(timer_wakes.load(Ordering::SeqCst), 1);
  assert_eq!(out.io_events, 0);
  assert_eq!(io_wakes.load(Ordering::SeqCst), 0);

  drop(write_fd);
  drop(read_fd);
}

#[test]
fn notify_breaks_blocking_poll_without_external_sources() {
  let driver = ReactorDriver::new().unwrap();
  let driver2 = driver.clone();

  let start = Instant::now();
  let handle = std::thread::spawn(move || driver2.poll(Some(Duration::from_secs(2))).unwrap());

  // Give the poll thread a chance to block.
  std::thread::sleep(Duration::from_millis(50));
  driver.notify().unwrap();

  let out = handle.join().unwrap();
  let elapsed = start.elapsed();

  assert!(elapsed < Duration::from_secs(1), "notify did not break poll promptly: {elapsed:?}");
  assert!(!out.did_work(), "notify-only wakeup should be reported as no work");
}

fn new_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  // SAFETY: fds are fresh from `pipe`.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((read, write))
}

fn set_nonblocking(fd: i32) -> std::io::Result<()> {
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags < 0 {
    return Err(std::io::Error::last_os_error());
  }
  if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}
