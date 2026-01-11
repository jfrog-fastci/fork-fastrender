#![cfg(target_os = "linux")]

use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::task::{Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use super::super::Interest;

struct CountWake {
  wakes: AtomicUsize,
}

impl CountWake {
  fn new() -> Arc<Self> {
    Arc::new(Self {
      wakes: AtomicUsize::new(0),
    })
  }

  fn waker(this: &Arc<Self>) -> Waker {
    Waker::from(Arc::clone(this))
  }

  fn count(&self) -> usize {
    self.wakes.load(Ordering::SeqCst)
  }
}

impl Wake for CountWake {
  fn wake(self: Arc<Self>) {
    self.wakes.fetch_add(1, Ordering::SeqCst);
  }
}

fn pipe() -> io::Result<(RawFd, RawFd)> {
  let mut fds = [0; 2];
  // SAFETY: syscall, `fds` is valid for two fds.
  let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
  if res < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((fds[0], fds[1]))
}

fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
  while !buf.is_empty() {
    // SAFETY: syscall, pointer is valid for buf.len() bytes.
    let res = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
    if res >= 0 {
      buf = &buf[res as usize..];
      continue;
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EINTR) {
      continue;
    }
    return Err(err);
  }
  Ok(())
}

fn close_fd(fd: RawFd) {
  // SAFETY: syscall.
  unsafe {
    libc::close(fd);
  }
}

#[test]
fn read_readiness_wakes_once() {
  let reactor = super::Reactor::new().unwrap();
  let (read_fd, write_fd) = pipe().unwrap();

  let wake = CountWake::new();
  let waker = CountWake::waker(&wake);

  reactor
    .register(read_fd, Interest::READABLE, &waker)
    .unwrap();

  let barrier = Arc::new(Barrier::new(2));
  let barrier2 = Arc::clone(&barrier);
  let writer = thread::spawn(move || {
    barrier2.wait();
    thread::sleep(Duration::from_millis(25));
    write_all(write_fd, b"x").unwrap();
    close_fd(write_fd);
  });

  barrier.wait();
  let outcome = reactor.poll(Some(Duration::from_secs(1))).unwrap();
  assert!(outcome.wakers_fired());
  assert_eq!(wake.count(), 1);

  writer.join().unwrap();
  close_fd(read_fd);
}

#[test]
fn notify_wakes_blocked_poll() {
  let reactor = super::Reactor::new().unwrap();
  let barrier = Arc::new(Barrier::new(2));
  let (tx, rx) = mpsc::channel();

  let reactor2 = reactor.clone();
  let barrier2 = Arc::clone(&barrier);
  thread::spawn(move || {
    barrier2.wait();
    let start = Instant::now();
    reactor2.poll(Some(Duration::from_secs(5))).unwrap();
    tx.send(start.elapsed()).unwrap();
  });

  barrier.wait();
  thread::sleep(Duration::from_millis(50));
  reactor.notify().unwrap();

  let elapsed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
  assert!(elapsed < Duration::from_secs(1), "poll returned too slowly: {elapsed:?}");
}

#[test]
fn deregister_prevents_wake() {
  let reactor = super::Reactor::new().unwrap();
  let (read_fd, write_fd) = pipe().unwrap();

  let wake = CountWake::new();
  let waker = CountWake::waker(&wake);

  reactor
    .register(read_fd, Interest::READABLE, &waker)
    .unwrap();
  reactor
    .deregister(read_fd, Interest::READABLE)
    .unwrap();

  let writer = thread::spawn(move || {
    thread::sleep(Duration::from_millis(25));
    write_all(write_fd, b"x").unwrap();
    close_fd(write_fd);
  });

  let outcome = reactor.poll(Some(Duration::from_millis(150))).unwrap();
  assert_eq!(outcome.io_events, 0);
  assert_eq!(wake.count(), 0);

  writer.join().unwrap();
  close_fd(read_fd);
}

#[test]
fn stale_event_does_not_wake_new_registration() {
  let reactor = super::Reactor::new().unwrap();

  let (read1, write1) = pipe().unwrap();
  let old_fd_num = read1;

  let wake_old = CountWake::new();
  let waker_old = CountWake::waker(&wake_old);
  reactor
    .register(read1, Interest::READABLE, &waker_old)
    .unwrap();

  // Hold the state lock so the poll thread gets stuck after `epoll_wait` returns but before it can
  // look up the entry and wake a waker.
  let mut state = reactor.inner.state.lock().unwrap_or_else(|e| e.into_inner());

  let (started_tx, started_rx) = mpsc::channel();
  let (done_tx, done_rx) = mpsc::channel();
  let reactor2 = reactor.clone();
  thread::spawn(move || {
    started_tx.send(()).unwrap();
    let res = reactor2.poll(Some(Duration::from_secs(5)));
    done_tx.send(res).unwrap();
  });

  started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  thread::sleep(Duration::from_millis(25));

  // Make the old fd readable, causing `epoll_wait` to return with a token containing the old
  // generation.
  write_all(write1, b"x").unwrap();
  thread::sleep(Duration::from_millis(25));

  // Swap registrations while the poll thread is blocked on the state lock.
  reactor
    .deregister_locked(&mut state, read1, Interest::READABLE)
    .unwrap();

  // Create a new pipe and swap its read end onto the old numeric fd. Doing this
  // with `dup2` avoids leaving `old_fd_num` temporarily free (which could allow
  // other concurrently-running tests to reuse it).
  let (read2, write2) = pipe().unwrap();
  // SAFETY: syscall.
  let duped = unsafe { libc::dup2(read2, old_fd_num) };
  assert_eq!(duped, old_fd_num);
  // `dup2` leaves `read2` open; close it so only `old_fd_num` refers to the new pipe.
  close_fd(read2);
  // `dup2` closed the old pipe's read end (`read1` / `old_fd_num`); close the write end too.
  close_fd(write1);
  let read2 = old_fd_num;

  let wake_new = CountWake::new();
  let waker_new = CountWake::waker(&wake_new);
  reactor
    .register_locked(&mut state, read2, Interest::READABLE, &waker_new)
    .unwrap();

  drop(state);

  // The poll thread should observe the old event, but must not wake the new registration due to
  // generation mismatch.
  let outcome = done_rx
    .recv_timeout(Duration::from_secs(1))
    .unwrap()
    .unwrap();
  assert_eq!(outcome.io_events, 1);

  assert_eq!(wake_new.count(), 0);

  // New readiness should still wake the new waker.
  write_all(write2, b"x").unwrap();
  reactor.poll(Some(Duration::from_secs(1))).unwrap();
  assert_eq!(wake_new.count(), 1);

  close_fd(read2);
  close_fd(write2);
}
