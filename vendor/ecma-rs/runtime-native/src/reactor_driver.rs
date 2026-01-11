//! Unified OS reactor + timer wheel driver for `rt_async_poll()`.
//!
//! [`ReactorDriver`] multiplexes:
//! - OS I/O readiness (epoll on Linux, kqueue on macOS/BSD) via [`crate::reactor`].
//! - Timer expirations via [`crate::timer_wheel`] (wrapped by [`crate::time::TimerDriver`]).
//! - Cross-thread wakeups via [`ReactorDriver::notify`].
//!
//! # `rt_async_poll()` integration contract
//!
//! The planned outer executor loop should use the driver like:
//! - If there are runnable tasks, call [`ReactorDriver::poll`] with
//!   `timeout = Some(Duration::ZERO)` to drain readiness without blocking.
//! - If there are no runnable tasks but there are registered fds/timers, call
//!   [`ReactorDriver::poll`] with `timeout = None` to wait until the next event.
//! - If there are no runnable tasks and no registered fds/timers, return `false`
//!   (idle) without calling into the driver.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Waker;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::reactor::Interest;
use crate::reactor::Reactor;
use crate::reactor::Token;
use crate::time::TimerDriver;
use crate::timer_wheel::TimerKey;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollOutcome {
  pub io_events: usize,
  pub timers_fired: usize,
}

impl PollOutcome {
  pub fn did_work(&self) -> bool {
    self.io_events != 0 || self.timers_fired != 0
  }
}

#[derive(Clone)]
pub struct ReactorDriver {
  inner: Arc<Inner>,
}

struct Inner {
  reactor: Mutex<Reactor>,
  reactor_waker: crate::reactor::Waker,
  io_wakers: Mutex<HashMap<Token, Waker>>,
  timers: Mutex<TimerDriver>,

  // Only one thread should block in `poll()` at a time. This avoids surprising
  // interactions where multiple pollers race to consume readiness and timers.
  poll_guard: Mutex<()>,

  // Indicates whether a thread is currently blocked (or about to block) inside
  // the OS poll call. Used to avoid "stale" wakeups when registrations happen on
  // the poll thread itself.
  is_polling: AtomicBool,
}

impl ReactorDriver {
  pub fn new() -> io::Result<Self> {
    let reactor = Reactor::new()?;
    let reactor_waker = reactor.waker();
    Ok(Self {
      inner: Arc::new(Inner {
        reactor: Mutex::new(reactor),
        reactor_waker,
        io_wakers: Mutex::new(HashMap::new()),
        timers: Mutex::new(TimerDriver::new()),
        poll_guard: Mutex::new(()),
        is_polling: AtomicBool::new(false),
      }),
    })
  }

  /// Returns `true` if there are external event sources registered (fds or timers).
  ///
  /// This intentionally ignores the internal cross-thread wakeup mechanism.
  pub fn has_external_sources(&self) -> bool {
    !self.inner.io_wakers.lock().is_empty() || !self.inner.timers.lock().is_empty()
  }

  pub fn notify(&self) -> io::Result<()> {
    self.inner.reactor_waker.wake()
  }

  pub fn register_fd(&self, fd: BorrowedFd<'_>, interest: Interest, waker: Waker) -> io::Result<Token> {
    let token = Token(fd.as_raw_fd() as usize);
    {
      let reactor = self.inner.reactor.lock();
      if self.inner.io_wakers.lock().contains_key(&token) {
        reactor.reregister(fd, token, interest)?;
      } else {
        reactor.register(fd, token, interest)?;
      }
    }
    self.inner.io_wakers.lock().insert(token, waker);

    // If another thread is blocked in `poll()`, wake it so it can observe the
    // updated registrations. (Registration itself is expected to be performed
    // on the poll thread; `notify()` is primarily for waking the poller when new
    // tasks become runnable.)
    if self.inner.is_polling.load(Ordering::SeqCst) {
      let _ = self.inner.reactor_waker.wake();
    }

    Ok(token)
  }

  pub fn deregister_fd(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
    let token = Token(fd.as_raw_fd() as usize);
    self.inner.reactor.lock().deregister(fd)?;
    self.inner.io_wakers.lock().remove(&token);
    if self.inner.is_polling.load(Ordering::SeqCst) {
      let _ = self.inner.reactor_waker.wake();
    }
    Ok(())
  }

  pub fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerKey {
    let mut timers = self.inner.timers.lock();
    let prev = timers.next_deadline();
    let key = timers.insert(deadline, waker);
    let next = timers.next_deadline();
    drop(timers);

    // If another thread is blocked in `poll()` and the next deadline moved
    // earlier, wake it so it can recompute its OS timeout.
    let became_earlier = match (prev, next) {
      (None, Some(_)) => true,
      (Some(p), Some(n)) => n < p,
      _ => false,
    };
    if became_earlier && self.inner.is_polling.load(Ordering::SeqCst) {
      let _ = self.inner.reactor_waker.wake();
    }

    key
  }

  pub fn cancel_timer(&self, key: TimerKey) -> bool {
    let mut timers = self.inner.timers.lock();
    let prev = timers.next_deadline();
    let removed = timers.cancel(key);
    let next = timers.next_deadline();
    drop(timers);

    if removed && next != prev && self.inner.is_polling.load(Ordering::SeqCst) {
      let _ = self.inner.reactor_waker.wake();
    }

    removed
  }

  /// Poll the driver for I/O and timer readiness.
  ///
  /// Timeout calculation:
  /// - `timeout` is first interpreted as the user-supplied maximum block time.
  /// - If the timer wheel has a next deadline, the actual OS poll timeout is
  ///   `min(timeout, time_until_next_deadline)`.
  /// - Sub-millisecond waits are rounded up by the OS backends (epoll uses
  ///   millisecond timeouts).
  pub fn poll(&self, timeout: Option<Duration>) -> io::Result<PollOutcome> {
    // Ensure a single poller at a time.
    let _poll_guard = self.inner.poll_guard.lock();

    let now = Instant::now();
    let timer_timeout = self.inner.timers.lock().time_until_next_deadline(now);
    let actual_timeout = min_timeout(timeout, timer_timeout);

    let polling_guard = PollingGuard::new(&self.inner.is_polling, actual_timeout);
    let mut events = Vec::new();
    let _n = self
      .inner
      .reactor
      .lock()
      .poll(&mut events, actual_timeout)?;
    drop(polling_guard);

    let mut io_events = 0usize;
    if !events.is_empty() {
      let io_wakers = self.inner.io_wakers.lock();
      for ev in events {
        if ev.token == Token::WAKE {
          continue;
        }
        if let Some(waker) = io_wakers.get(&ev.token) {
          // We keep the waker registered; the corresponding future will update
          // it on its next poll if needed.
          waker.wake_by_ref();
          io_events += 1;
        }
      }
    }

    let expired = self.inner.timers.lock().poll_expired(Instant::now());
    let timers_fired = expired.len();
    for w in expired {
      w.wake();
    }

    Ok(PollOutcome {
      io_events,
      timers_fired,
    })
  }
}

fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
  match (a, b) {
    (None, None) => None,
    (Some(x), None) => Some(x),
    (None, Some(y)) => Some(y),
    (Some(x), Some(y)) => Some(std::cmp::min(x, y)),
  }
}

struct PollingGuard<'a> {
  flag: &'a AtomicBool,
}

impl<'a> PollingGuard<'a> {
  fn new(flag: &'a AtomicBool, timeout: Option<Duration>) -> Option<Self> {
    // Non-blocking polls don't need to be interrupted by `notify()`.
    if matches!(timeout, Some(d) if d.is_zero()) {
      return None;
    }
    flag.store(true, Ordering::SeqCst);
    Some(Self { flag })
  }
}

impl Drop for PollingGuard<'_> {
  fn drop(&mut self) {
    self.flag.store(false, Ordering::SeqCst);
  }
}
