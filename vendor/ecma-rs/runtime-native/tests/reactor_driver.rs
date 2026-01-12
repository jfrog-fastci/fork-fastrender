use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::task::RawWaker;
use std::task::RawWakerVTable;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use runtime_native::clock::VirtualClock;
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
fn register_fd_requires_nonblocking() {
  let driver = ReactorDriver::new().unwrap();

  let (read_fd, write_fd) = new_pipe().unwrap();

  // `pipe()` returns a blocking fd by default.
  let io_wakes = Arc::new(AtomicUsize::new(0));
  let err = driver
    .register_fd(read_fd.as_fd(), Interest::READABLE, counting_waker(io_wakes.clone()))
    .unwrap_err();
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "got {err:?}");
  assert_eq!(io_wakes.load(Ordering::SeqCst), 0);

  // Ensure the failed registration did not leave a stale mapping behind by setting O_NONBLOCK and
  // registering again.
  set_nonblocking(read_fd.as_raw_fd()).unwrap();
  let _token = driver
    .register_fd(read_fd.as_fd(), Interest::READABLE, counting_waker(io_wakes.clone()))
    .unwrap();

  let b = [0x1u8; 1];
  let rc = unsafe { libc::write(write_fd.as_raw_fd(), b.as_ptr().cast::<libc::c_void>(), 1) };
  assert_eq!(rc, 1);
  let out = driver.poll(Some(Duration::from_secs(1))).unwrap();
  assert_eq!(out.io_events, 1);
  assert_eq!(io_wakes.load(Ordering::SeqCst), 1);

  driver.deregister_fd(read_fd.as_fd()).unwrap();
  drop(write_fd);
  drop(read_fd);
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
fn virtual_time_allows_long_timeouts_without_wall_clock_waiting() {
  let clock = Arc::new(VirtualClock::new());
  let driver = ReactorDriver::new_with_clock(clock.clone()).unwrap();

  let fired = Arc::new(AtomicUsize::new(0));
  driver.register_timer(driver.now() + Duration::from_secs(30), counting_waker(fired.clone()));
  clock.advance(Duration::from_secs(30));

  let out = driver.poll(Some(Duration::from_millis(250))).unwrap();
  assert_eq!(out.timers_fired, 1);
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

#[test]
fn token_is_not_reused_across_fd_reuse() {
  let driver = ReactorDriver::new().unwrap();

  let (read1, write1) = new_pipe().unwrap();
  let old_fd = read1.as_raw_fd();
  set_nonblocking(old_fd).unwrap();

  let token1 = driver
    .register_fd(read1.as_fd(), Interest::READABLE, counting_waker(Arc::new(AtomicUsize::new(0))))
    .unwrap();
  driver.deregister_fd(read1.as_fd()).unwrap();

  // Avoid `OwnedFd` closing `old_fd` after we repurpose it with `dup2`.
  let old_fd = read1.into_raw_fd();

  // Create a new pipe and swap its read end onto the old numeric fd. Doing this with `dup2` avoids
  // leaving `old_fd` temporarily free (which could allow other concurrently-running tests to reuse
  // it).
  let (read2, write2) = new_pipe().unwrap();
  set_nonblocking(read2.as_raw_fd()).unwrap();

  let read2_raw = read2.into_raw_fd();
  let duped = unsafe { libc::dup2(read2_raw, old_fd) };
  assert_eq!(duped, old_fd);
  unsafe {
    libc::close(read2_raw);
  }

  // The old pipe is now reader-less; close the writer to avoid EPIPE/SIGPIPE on drop.
  drop(write1);

  // SAFETY: `dup2` produced an owned fd at `old_fd`.
  let read2 = unsafe { OwnedFd::from_raw_fd(old_fd) };

  let wakes = Arc::new(AtomicUsize::new(0));
  let token2 = driver
    .register_fd(read2.as_fd(), Interest::READABLE, counting_waker(wakes.clone()))
    .unwrap();

  assert_ne!(token1, token2, "token should not be derived from the raw fd number");

  // Ensure the new registration works.
  let b = [0x1u8; 1];
  let rc = unsafe { libc::write(write2.as_raw_fd(), b.as_ptr().cast::<libc::c_void>(), 1) };
  assert_eq!(rc, 1);
  let out = driver.poll(Some(Duration::from_secs(1))).unwrap();
  assert_eq!(out.io_events, 1);
  assert_eq!(wakes.load(Ordering::SeqCst), 1);

  let _ = driver.deregister_fd(read2.as_fd());
  drop(read2);
  drop(write2);
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
