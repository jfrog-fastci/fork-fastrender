#![cfg(unix)]

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
fn timeout_no_events() {
  let mut reactor = Reactor::new().unwrap();
  let mut events = Vec::new();

  let start = Instant::now();
  reactor
    .poll(&mut events, Some(Duration::from_millis(50)))
    .unwrap();

  assert!(events.is_empty(), "expected 0 events, got {events:?}");
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "poll appears to have blocked too long: {:?}",
    start.elapsed()
  );
}

#[test]
fn edge_trigger_requires_drain() {
  let (read, write) = pipe().unwrap();
  set_nonblocking(read.as_raw_fd()).unwrap();

  let mut reactor = Reactor::new().unwrap();
  reactor
    .register(read.as_fd(), Token(10), Interest::READABLE)
    .unwrap();

  // Make it readable.
  let b = [0x1u8];
  let rc = unsafe { libc::write(write.as_raw_fd(), b.as_ptr() as *const libc::c_void, 1) };
  assert_eq!(rc, 1);

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();
  assert!(
    events.iter().any(|e| e.token == Token(10) && e.readable),
    "expected first readable event, got {events:?}"
  );

  // If the reactor is edge-triggered, polling again without draining should *not* produce another
  // readable event (the fd is still readable, but there was no new edge).
  reactor
    .poll(&mut events, Some(Duration::from_millis(50)))
    .unwrap();
  assert!(
    events.is_empty(),
    "expected no events without draining (edge-triggered), got {events:?}"
  );

  // Drain until WouldBlock, then write again to produce a new edge.
  drain_read_nonblocking(read.as_raw_fd()).unwrap();
  let rc = unsafe { libc::write(write.as_raw_fd(), b.as_ptr() as *const libc::c_void, 1) };
  assert_eq!(rc, 1);

  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();
  assert!(
    events.iter().any(|e| e.token == Token(10) && e.readable),
    "expected readable event after draining + new write, got {events:?}"
  );
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
fn token_wake_is_reserved() {
  let (read, _write) = pipe().unwrap();
  set_nonblocking(read.as_raw_fd()).unwrap();

  let mut reactor = Reactor::new().unwrap();
  let err = reactor
    .register(read.as_fd(), Token::WAKE, Interest::READABLE)
    .expect_err("expected Token::WAKE registration to fail");

  assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "got {err:?}");
}

#[test]
fn read_ready_pipe() {
  let (read, write) = pipe().unwrap();
  set_nonblocking(read.as_raw_fd()).unwrap();

  let mut reactor = Reactor::new().unwrap();
  reactor
    .register(read.as_fd(), Token(1), Interest::READABLE)
    .unwrap();

  // Make it readable.
  let b = [0x1u8];
  let rc = unsafe { libc::write(write.as_raw_fd(), b.as_ptr() as *const libc::c_void, 1) };
  assert_eq!(rc, 1);

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();

  let ev = events
    .iter()
    .find(|e| e.token == Token(1))
    .expect("missing event for token 1");
  assert!(ev.readable, "expected readable event, got {ev:?}");

  // Drain to satisfy edge-triggered contract.
  drain_read_nonblocking(read.as_raw_fd()).unwrap();
}

#[test]
fn modify_interests() {
  let (a, b) = socketpair().unwrap();
  set_nonblocking(a.as_raw_fd()).unwrap();
  set_nonblocking(b.as_raw_fd()).unwrap();

  // Fill `a`'s send buffer so it becomes not-writable.
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
    .register(a.as_fd(), Token(2), Interest::READABLE)
    .unwrap();

  // Now switch to writable interest.
  reactor
    .reregister(a.as_fd(), Token(2), Interest::WRITABLE)
    .unwrap();

  // Drain some bytes from `b` to make `a` writable again.
  drain_read_nonblocking(b.as_raw_fd()).unwrap();

  let mut events = Vec::new();
  reactor
    .poll(&mut events, Some(Duration::from_secs(1)))
    .unwrap();

  let ev = events
    .iter()
    .find(|e| e.token == Token(2))
    .expect("missing event for token 2");
  assert!(ev.writable, "expected writable event, got {ev:?}");
}

#[test]
fn event_merge_by_token() {
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
fn hup_eof_semantics() {
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
fn waker_interrupts_poll() {
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
    "wake did not interrupt poll promptly: {:?}",
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
