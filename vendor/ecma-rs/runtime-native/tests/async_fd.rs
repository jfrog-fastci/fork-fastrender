use runtime_native::io::AsyncFd;
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
fn pipe_readable_after_write() {
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
  let (rfd, wfd) = pipe().unwrap();
  let afd = AsyncFd::new(wfd);

  let start = Instant::now();
  block_on_rt(async { afd.writable().await.unwrap() }, Duration::from_secs(1));
  assert!(
    start.elapsed() < Duration::from_millis(250),
    "writable did not resolve promptly"
  );

  drop(rfd);
}

#[test]
fn drop_cancels_waiter() {
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
