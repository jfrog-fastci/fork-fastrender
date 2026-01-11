//! Kqueue backend regression tests for the `runtime-native` reactor.
//!
//! These are **platform-gated** so Linux CI can still build the crate while allowing local
//! validation of the kqueue backend on macOS/BSD.
//!
//! Run locally (macOS/BSD):
//!
//! ```bash
//! bash vendor/ecma-rs/scripts/cargo_agent.sh test -p runtime-native --test reactor_kqueue
//! ```
//!
//! Pipe-based wake fallback coverage lives in `reactor_kqueue_pipe_wake.rs`:
//!
//! ```bash
//! bash vendor/ecma-rs/scripts/cargo_agent.sh test -p runtime-native \
//!   --test reactor_kqueue_pipe_wake --features force_pipe_wake
//! ```
#![cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use runtime_native::reactor::{Interest, Reactor, Token};

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags == -1 {
    return Err(io::Error::last_os_error());
  }
  let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: libc returned valid fds.
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((read, write))
}

fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  let a = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let b = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((a, b))
}

fn write_all_nonblocking(fd: RawFd, mut buf: &[u8]) -> io::Result<usize> {
  let mut written = 0;
  while !buf.is_empty() {
    let rc = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if rc == -1 {
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::WouldBlock {
        return Ok(written);
      }
      return Err(err);
    }
    let n = rc as usize;
    written += n;
    buf = &buf[n..];
  }
  Ok(written)
}

fn drain_read_nonblocking(fd: RawFd) -> io::Result<usize> {
  let mut total = 0;
  let mut buf = [0u8; 4096];
  loop {
    let rc = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if rc == -1 {
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::WouldBlock {
        return Ok(total);
      }
      return Err(err);
    }
    if rc == 0 {
      return Ok(total);
    }
    total += rc as usize;
  }
}

#[test]
fn register_requires_nonblocking() {
  let (read, _write) = pipe().unwrap();

  let mut reactor = Reactor::new().unwrap();
  let err = reactor
    .register(read.as_fd(), Token(11), Interest::READABLE)
    .expect_err("expected registering a blocking fd to fail");

  assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "got {err:?}");
}

#[test]
fn waker_interrupts_poll_some_timeout() {
  let mut reactor = Reactor::new().unwrap();
  let waker = reactor.waker();

  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    waker.wake().unwrap();
  });

  let start = Instant::now();
  let mut events = Vec::new();
  reactor.poll(&mut events, Some(Duration::from_secs(5))).unwrap();

  assert!(
    start.elapsed() < Duration::from_secs(1),
    "wake did not interrupt poll(Some(..)) promptly: {:?}",
    start.elapsed()
  );
  assert!(
    events.iter().any(|e| e.token == Token::WAKE),
    "expected wake event, got {events:?}"
  );
}

#[test]
fn waker_interrupts_poll_none_timeout() {
  let mut reactor = Reactor::new().unwrap();
  let waker = reactor.waker();

  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    waker.wake().unwrap();
  });

  let start = Instant::now();
  let mut events = Vec::new();
  reactor.poll(&mut events, None).unwrap();

  assert!(
    start.elapsed() < Duration::from_secs(1),
    "wake did not interrupt poll(None) promptly: {:?}",
    start.elapsed()
  );
  assert!(
    events.iter().any(|e| e.token == Token::WAKE),
    "expected wake event, got {events:?}"
  );
}

#[test]
fn event_merge_by_token_read_write() {
  let (a, b) = socketpair().unwrap();
  set_nonblocking(a.as_raw_fd()).unwrap();
  set_nonblocking(b.as_raw_fd()).unwrap();

  // Make `a` non-writable before registering (so we can generate a writability edge).
  let payload = vec![0u8; 16 * 1024];
  loop {
    match write_all_nonblocking(a.as_raw_fd(), &payload) {
      Ok(0) => break,
      Ok(_) => continue,
      Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
      Err(e) => panic!("unexpected write error: {e:?}"),
    }
  }

  let mut reactor = Reactor::new().unwrap();
  reactor
    .register(
      a.as_fd(),
      Token(3),
      Interest::READABLE | Interest::WRITABLE,
    )
    .unwrap();

  // Cause readability edge: write 1 byte from b -> a.
  let one = [0xAAu8];
  let rc = unsafe { libc::write(b.as_raw_fd(), one.as_ptr() as *const libc::c_void, 1) };
  assert_eq!(rc, 1);

  // Cause writability edge: drain some buffered bytes from b (the data we wrote while filling).
  drain_read_nonblocking(b.as_raw_fd()).unwrap();

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();

  let matching: Vec<_> = events.iter().filter(|e| e.token == Token(3)).collect();
  assert_eq!(
    matching.len(),
    1,
    "expected exactly one event for token 3 (merged), got {events:?}"
  );
  let ev = matching[0];
  assert!(ev.readable, "expected readable, got {ev:?}");
  assert!(ev.writable, "expected writable, got {ev:?}");
}

#[test]
fn eof_hup_semantics_pipe() {
  let (read, write) = pipe().unwrap();
  set_nonblocking(read.as_raw_fd()).unwrap();

  let mut reactor = Reactor::new().unwrap();
  reactor
    .register(read.as_fd(), Token(4), Interest::READABLE)
    .unwrap();

  drop(write); // close writer => EOF on read end.

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();

  let ev = events
    .iter()
    .find(|e| e.token == Token(4))
    .expect("missing event for token 4");
  assert!(ev.readable, "expected readable on EOF, got {ev:?}");
  assert!(ev.read_closed, "expected read_closed on EOF, got {ev:?}");
}

#[test]
fn eof_hup_semantics_socketpair() {
  let (a, b) = socketpair().unwrap();
  set_nonblocking(a.as_raw_fd()).unwrap();
  set_nonblocking(b.as_raw_fd()).unwrap();

  // Prevent an initial writable-ready event by filling `a`'s send buffer first.
  let payload = vec![0u8; 16 * 1024];
  loop {
    match write_all_nonblocking(a.as_raw_fd(), &payload) {
      Ok(0) => break,
      Ok(_) => continue,
      Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
      Err(e) => panic!("unexpected write error: {e:?}"),
    }
  }

  let mut reactor = Reactor::new().unwrap();
  reactor
    .register(
      a.as_fd(),
      Token(5),
      Interest::READABLE | Interest::WRITABLE,
    )
    .unwrap();

  drop(b); // full close => EOF/HUP semantics on `a`.

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();

  let ev = events
    .iter()
    .find(|e| e.token == Token(5))
    .expect("missing event for token 5");

  assert!(ev.readable, "expected readable on peer close, got {ev:?}");
  assert!(ev.read_closed, "expected read_closed on peer close, got {ev:?}");
  assert!(ev.writable, "expected writable on peer close, got {ev:?}");
  assert!(ev.write_closed, "expected write_closed on peer close, got {ev:?}");
}

#[test]
fn waker_no_loss_stress() {
  let mut reactor = Reactor::new().unwrap();
  let waker = reactor.waker();

  let (req_tx, req_rx) = std::sync::mpsc::channel::<()>();
  let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();

  std::thread::spawn(move || {
    for _ in req_rx {
      for _ in 0..50 {
        waker.wake().unwrap();
      }
      ack_tx.send(()).unwrap();
    }
  });

  let mut events = Vec::new();
  for _ in 0..100 {
    req_tx.send(()).unwrap();

    reactor
      .poll(&mut events, Some(Duration::from_secs(1)))
      .unwrap();
    assert!(
      events.iter().any(|e| e.token == Token::WAKE),
      "expected wake event, got {events:?}"
    );

    ack_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  }
}
