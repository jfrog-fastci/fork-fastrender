use runtime_native::io::AsyncFd;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{async_rt, rt_async_poll};
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
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

fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  set_nonblocking(fds[0])?;
  set_nonblocking(fds[1])?;
  // Safety: `socketpair` returns new, owned file descriptors.
  let a = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let b = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((a, b))
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

extern "C" fn set_timeout_flag(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

fn block_on_rt<F: Future>(fut: F, timeout: Duration) -> F::Output {
  let timed_out = Box::new(AtomicBool::new(false));
  let timed_out_ptr: *mut AtomicBool = Box::into_raw(timed_out);

  let timer_id = async_rt::global().schedule_timer(
    Instant::now() + timeout,
    async_rt::Task::new(set_timeout_flag, timed_out_ptr.cast::<u8>()),
  );

  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke.clone());
  let mut cx = Context::from_waker(&waker);
  let mut fut = Box::pin(fut);
  let mut ever_woken = false;

  loop {
    match fut.as_mut().poll(&mut cx) {
      Poll::Ready(out) => {
        let _ = async_rt::global().cancel_timer(timer_id);
        unsafe {
          drop(Box::from_raw(timed_out_ptr));
        }
        ever_woken |= woke.load(Ordering::SeqCst);
        assert!(ever_woken, "future resolved without being woken by the reactor");
        return out;
      }
      Poll::Pending => {
        rt_async_poll();
        if woke.swap(false, Ordering::SeqCst) {
          ever_woken = true;
        }
        let timed_out = unsafe { &*timed_out_ptr };
        if timed_out.load(Ordering::SeqCst) {
          panic!("timed out waiting for future");
        }
      }
    }
  }
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

fn write_byte(fd: RawFd) {
  let byte: u8 = 1;
  let rc = unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
  assert_eq!(rc, 1, "write failed: {}", io::Error::last_os_error());
}

#[test]
fn blocking_fd_is_rejected_and_does_not_leak_registration() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe_blocking().unwrap();
  let afd = AsyncFd::new(rfd);

  // Poll once to force a registration attempt (which must fail for blocking fds).
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

  // Ensure the failure did not leave a stale registration by setting O_NONBLOCK and re-awaiting.
  set_nonblocking(afd.as_raw_fd()).unwrap();

  let writer = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(10));
    write_byte(wfd.as_raw_fd());
  });

  block_on_rt(async { afd.readable().await.unwrap() }, Duration::from_secs(1));
  writer.join().unwrap();
}

#[test]
fn pipe_readable_after_write() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe().unwrap();
  let afd = AsyncFd::new(rfd);

  let writer = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(10));
    write_byte(wfd.as_raw_fd());
  });

  block_on_rt(async { afd.readable().await.unwrap() }, Duration::from_secs(1));
  writer.join().unwrap();
}

#[test]
fn writable_is_immediately_ready_for_pipe() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe().unwrap();
  let afd = AsyncFd::new(wfd);

  block_on_rt(async { afd.writable().await.unwrap() }, Duration::from_secs(1));

  drop(rfd);
}

#[test]
fn hup_counts_as_writable_readiness() {
  let _rt = TestRuntimeGuard::new();
  let (a, b) = socketpair().unwrap();
  let afd = AsyncFd::new(a);

  // Closing the peer end should cause epoll to report HUP/ERR. `AsyncFd` treats those events as
  // readiness, so writable() must resolve even though an eventual write would fail (EPIPE).
  drop(b);

  block_on_rt(async { afd.writable().await.unwrap() }, Duration::from_secs(1));
}

#[test]
fn drop_cancels_waiter() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe().unwrap();
  let afd = AsyncFd::new(rfd);

  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke.clone());
  let mut cx = Context::from_waker(&waker);

  let mut fut = Box::pin(afd.readable());
  assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

  drop(fut);
  write_byte(wfd.as_raw_fd());

  // Ensure we don't block indefinitely if other tests leave the global runtime non-idle.
  let timed_out = Box::new(AtomicBool::new(false));
  let timed_out_ptr = Box::into_raw(timed_out);
  let timer_id = async_rt::global().schedule_timer(
    Instant::now() + Duration::from_millis(50),
    async_rt::Task::new(set_timeout_flag, timed_out_ptr.cast::<u8>()),
  );

  let _ = rt_async_poll();
  let _ = async_rt::global().cancel_timer(timer_id);
  unsafe {
    drop(Box::from_raw(timed_out_ptr));
  }

  assert!(!woke.load(Ordering::SeqCst), "dropped waiter was spuriously woken");
}

#[test]
fn repoll_replaces_waker() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, wfd) = pipe().unwrap();
  let afd = AsyncFd::new(rfd);

  let woke1 = Arc::new(AtomicBool::new(false));
  let waker1 = flag_waker(woke1.clone());
  let mut cx1 = Context::from_waker(&waker1);

  let woke2 = Arc::new(AtomicBool::new(false));
  let waker2 = flag_waker(woke2.clone());
  let mut cx2 = Context::from_waker(&waker2);

  let mut fut = Box::pin(afd.readable());
  assert!(matches!(fut.as_mut().poll(&mut cx1), Poll::Pending));
  assert!(matches!(fut.as_mut().poll(&mut cx2), Poll::Pending));

  write_byte(wfd.as_raw_fd());

  let deadline = Instant::now() + Duration::from_secs(1);
  while !woke2.load(Ordering::SeqCst) {
    assert!(Instant::now() < deadline, "timed out waiting for waker wake");
    rt_async_poll();
  }

  assert!(
    !woke1.load(Ordering::SeqCst),
    "first waker was woken after being replaced"
  );
}

#[test]
fn drop_last_waiter_deregisters_fd() {
  let _rt = TestRuntimeGuard::new();
  let (rfd, _wfd) = pipe().unwrap();
  let afd = AsyncFd::new(rfd);

  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke);
  let mut cx = Context::from_waker(&waker);

  let fut = Box::pin(afd.readable());
  let mut fut = fut;
  assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
  drop(fut);

  // The drop path schedules cleanup work (deregister + Arc release) via a microtask. Make sure the
  // runtime becomes idle afterwards; if a stale reactor watcher remains registered, `rt_async_poll`
  // will report pending work even after this timer fires.
  let fired = Box::new(AtomicBool::new(false));
  let fired_ptr: *mut AtomicBool = Box::into_raw(fired);
  async_rt::global().schedule_timer(
    Instant::now() + Duration::from_millis(20),
    async_rt::Task::new(set_timeout_flag, fired_ptr.cast::<u8>()),
  );

  // First tick flushes the microtask (and returns early because the timer is pending).
  let _ = rt_async_poll();
  // Second tick waits for the timer, then should observe an idle reactor.
  let pending = rt_async_poll();

  let fired = unsafe { &*fired_ptr };
  assert!(fired.load(Ordering::SeqCst), "timer did not fire");
  unsafe {
    drop(Box::from_raw(fired_ptr));
  }

  assert!(
    !pending,
    "async runtime still had pending work after dropping last AsyncFd waiter"
  );
}
