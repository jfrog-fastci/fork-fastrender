//! Pipe-based kqueue wake regression tests.
//!
//! The kqueue reactor prefers `EVFILT_USER`, but can fall back to a portable pipe-based wakeup.
//! These tests force the pipe path via the `force_pipe_wake` feature.
//!
//! Run locally (macOS/BSD):
//!
//! ```bash
//! RUSTFLAGS="-C force-frame-pointers=yes" \
//!   bash vendor/ecma-rs/scripts/cargo_agent.sh test -p runtime-native \
//!   --test reactor_kqueue_pipe_wake --features force_pipe_wake
//! ```
#![cfg(all(
  feature = "force_pipe_wake",
  any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
  )
))]

use std::time::{Duration, Instant};

use runtime_native::reactor::{Reactor, Token};

#[test]
fn pipe_waker_interrupts_poll_some_timeout() {
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
fn pipe_waker_interrupts_poll_none_timeout() {
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
fn pipe_waker_drain_no_loss_stress() {
  // This mirrors `reactor_conformance::waker_no_loss_stress`, but specifically exercises the
  // pipe-based wake path. It's sensitive to drain bugs: leaving the pipe readable breaks
  // `EV_CLEAR` edge semantics and can cause the reactor to block forever.
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
