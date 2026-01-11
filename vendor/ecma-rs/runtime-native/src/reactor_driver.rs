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
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Waker;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::clock::{Clock, RealClock};
use crate::reactor::Interest;
use crate::reactor::Reactor;
use crate::reactor::Token;
use crate::sync::GcAwareMutex;
use crate::threading;
use crate::time::TimerDriver;
use crate::timer_wheel::TimerKey;

struct ClockState {
  clock: Arc<dyn Clock>,
  base: Instant,
}

impl ClockState {
  #[inline]
  fn now_std(&self) -> Instant {
    self
      .base
      .checked_add(self.clock.now())
      .unwrap_or_else(Instant::now)
  }
}

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
  reactor: GcAwareMutex<Reactor>,
  reactor_waker: crate::reactor::Waker,
  next_token: AtomicUsize,
  io: GcAwareMutex<IoState>,
  timers: GcAwareMutex<TimerDriver>,
  clock: ArcSwap<ClockState>,

  // Only one thread should block in `poll()` at a time. This avoids surprising
  // interactions where multiple pollers race to consume readiness and timers.
  poll_guard: GcAwareMutex<()>,

  // Indicates whether a thread is currently blocked (or about to block) inside
  // the OS poll call. Used to avoid "stale" wakeups when registrations happen on
  // the poll thread itself.
  is_polling: AtomicBool,
}

#[derive(Default)]
struct IoState {
  by_fd: HashMap<RawFd, Token>,
  wakers: HashMap<Token, Waker>,
}

impl Inner {
  fn alloc_token(&self) -> io::Result<Token> {
    self
      .next_token
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        if cur == 0 || cur == Token::WAKE.0 {
          return None;
        }
        Some(cur + 1)
      })
      .map(Token)
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "reactor token space exhausted"))
  }
}

impl ReactorDriver {
  pub fn new() -> io::Result<Self> {
    let base = Instant::now();
    let clock: Arc<dyn Clock> = Arc::new(RealClock::with_start(base));
    Self::new_inner(clock, base)
  }

  pub fn new_with_clock(clock: Arc<dyn Clock>) -> io::Result<Self> {
    let now = Instant::now();
    let base = now.checked_sub(clock.now()).unwrap_or(now);
    Self::new_inner(clock, base)
  }

  fn new_inner(clock: Arc<dyn Clock>, base: Instant) -> io::Result<Self> {
    let reactor = Reactor::new()?;
    let reactor_waker = reactor.waker();
    Ok(Self {
      inner: Arc::new(Inner {
        reactor: GcAwareMutex::new(reactor),
        reactor_waker,
        next_token: AtomicUsize::new(1),
        io: GcAwareMutex::new(IoState::default()),
        timers: GcAwareMutex::new(TimerDriver::new_at(base)),
        clock: ArcSwap::from_pointee(ClockState { clock, base }),
        poll_guard: GcAwareMutex::new(()),
        is_polling: AtomicBool::new(false),
      }),
    })
  }

  /// Returns `true` if there are external event sources registered (fds or timers).
  ///
  /// This intentionally ignores the internal cross-thread wakeup mechanism.
  pub fn has_external_sources(&self) -> bool {
    !self.inner.io.lock().wakers.is_empty() || !self.inner.timers.lock().is_empty()
  }

  pub fn now(&self) -> Instant {
    self.inner.clock.load().now_std()
  }

  pub fn notify(&self) -> io::Result<()> {
    self.inner.reactor_waker.wake()
  }

  pub fn register_fd(&self, fd: BorrowedFd<'_>, interest: Interest, waker: Waker) -> io::Result<Token> {
    let raw_fd = fd.as_raw_fd();

    let reactor = self.inner.reactor.lock();
    let mut io_state = self.inner.io.lock();

    // `Token` values must remain stable across any OS wait call; using the raw fd number directly
    // allows token reuse if the OS recycles the fd (and a readiness event from the old registration
    // is still being processed). Use an internal monotonic token generator instead.
    let (mut token, mut is_new) = match io_state.by_fd.get(&raw_fd).copied() {
      Some(token) => (token, false),
      None => {
        let token = self.inner.alloc_token()?;
        io_state.by_fd.insert(raw_fd, token);
        (token, true)
      }
    };

    let mut res = if is_new {
      reactor.register(fd, token, interest)
    } else {
      reactor.reregister(fd, token, interest)
    };

    // If the driver state got out of sync with the OS reactor (e.g. the fd was
    // closed without deregistration and then re-opened/reused), attempt to recover
    // by allocating a fresh token and re-registering.
    if !is_new && matches!(&res, Err(err) if err.kind() == io::ErrorKind::NotFound) {
      io_state.by_fd.remove(&raw_fd);
      io_state.wakers.remove(&token);
      token = self.inner.alloc_token()?;
      io_state.by_fd.insert(raw_fd, token);
      is_new = true;
      res = reactor.register(fd, token, interest);
    }

    if let Err(err) = res {
      // Roll back the logical registration if we inserted a new token mapping.
      if is_new {
        io_state.by_fd.remove(&raw_fd);
      }
      return Err(err);
    }

    io_state.wakers.insert(token, waker);
    drop(io_state);
    drop(reactor);

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
    let raw_fd = fd.as_raw_fd();

    let reactor = self.inner.reactor.lock();
    let mut io_state = self.inner.io.lock();
    if let Some(token) = io_state.by_fd.remove(&raw_fd) {
      io_state.wakers.remove(&token);
    }
    drop(io_state);
    reactor.deregister(fd)?;
    drop(reactor);
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

    let now = self.now();
    let timer_timeout = self.inner.timers.lock().time_until_next_deadline(now);
    let actual_timeout = min_timeout(timeout, timer_timeout);

    let mut polling_guard = PollingGuard::new(&self.inner.is_polling, actual_timeout);
    let mut events = Vec::new();
    let poll_res = if matches!(actual_timeout, Some(d) if d.is_zero()) {
      self.inner.reactor.lock().poll(&mut events, actual_timeout)
    } else {
      // While blocked in the OS poll syscall (`epoll_wait`/`kevent`), treat this thread as parked so
      // stop-the-world GC does not wait for it to reach a cooperative safepoint poll.
      let parked = threading::ParkedGuard::new();
      let res = self.inner.reactor.lock().poll(&mut events, actual_timeout);
      // Clear the `is_polling` flag before potentially blocking while un-parking.
      drop(polling_guard.take());
      drop(parked);
      res
    };
    let _n = poll_res?;
    drop(polling_guard);

    let io_wakers: Vec<Waker> = if events.is_empty() {
      Vec::new()
    } else {
      let io_state = self.inner.io.lock();
      events
        .into_iter()
        .filter_map(|ev| {
          if ev.token == Token::WAKE {
            None
          } else {
            io_state.wakers.get(&ev.token).cloned()
          }
        })
        .collect()
    };
    let io_events = io_wakers.len();
    for waker in io_wakers {
      waker.wake();
    }

    let expired = self.inner.timers.lock().poll_expired(self.now());
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn reactor_driver_poll_guard_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let driver = ReactorDriver::new().expect("failed to construct reactor driver");

    std::thread::scope(|scope| {
      // Thread A holds the poll_guard lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to acquire poll_guard while it is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

      let driver_a = driver.clone();
      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = driver_a.inner.poll_guard.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire poll_guard");

      let driver_c = driver.clone();
      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();
        c_start_rx.recv().unwrap();

        let _guard = driver_c.inner.poll_guard.lock();
        c_done_tx.send(()).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the lock before starting STW.
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
      let start = Instant::now();
      loop {
        let mut native_safe = false;
        threading::registry::for_each_thread(|t| {
          if t.id() == c_id {
            native_safe = t.is_native_safe();
          }
        });

        if native_safe {
          break;
        }
        if start.elapsed() > TIMEOUT {
          panic!("thread C did not enter a GC-safe region while blocked on poll_guard");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      struct ResumeOnDrop;
      impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
          crate::threading::safepoint::rt_gc_resume_world();
        }
      }
      let _resume = ResumeOnDrop;

      // Let thread A release the lock and reach the safepoint.
      a_release_tx.send(()).unwrap();

      assert!(
        crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
        "world failed to stop within timeout; poll_guard contention must not block STW"
      );

      // Resume the world so the contending lock acquisition can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should finish after world is resumed");
    });
  }
}
