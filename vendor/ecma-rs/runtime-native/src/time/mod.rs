use crate::async_rt;
use crate::async_rt::Task;
use crate::sync::GcAwareMutex;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

/// Sleep for `duration`.
///
/// `sleep(Duration::ZERO)` completes immediately without registering a timer.
pub fn sleep(duration: Duration) -> Sleep {
  let now = async_rt::global().now();
  let deadline = now.checked_add(duration).unwrap_or(now);
  sleep_until(deadline)
}

/// Sleep until `deadline`.
///
/// If `deadline <= async_rt::global().now()`, the returned [`Sleep`] completes immediately without
/// registering a timer.
pub fn sleep_until(deadline: Instant) -> Sleep {
  Sleep::new(deadline)
}

/// Run `fut` and error if it does not complete within `dur`.
pub fn timeout<F: Future>(dur: Duration, fut: F) -> Timeout<F> {
  Timeout {
    fut,
    sleep: sleep(dur),
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutError;

impl std::fmt::Display for TimeoutError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("deadline has elapsed")
  }
}

impl std::error::Error for TimeoutError {}

// -------------------------------------------------------------------------------------------------
// Sleep
// -------------------------------------------------------------------------------------------------

static NEXT_REGISTRATION_KEY: AtomicU64 = AtomicU64::new(1);

static REGISTRY: Lazy<GcAwareMutex<HashMap<u64, Arc<SleepShared>>>> =
  Lazy::new(|| GcAwareMutex::new(HashMap::new()));

#[derive(Debug)]
struct SleepShared {
  active: AtomicBool,
  fired: AtomicBool,
  waker: GcAwareMutex<Option<Waker>>,
}

impl SleepShared {
  fn update_waker(&self, waker: &Waker) {
    let mut slot = self.waker.lock();
    match &*slot {
      Some(existing) if existing.will_wake(waker) => {}
      _ => *slot = Some(waker.clone()),
    }
  }

  fn wake(&self) {
    let waker = self.waker.lock().take();
    if let Some(waker) = waker {
      waker.wake();
    }
  }
}

extern "C" fn on_timer_fire(data: *mut u8) {
  let key = data as usize as u64;
  let shared = REGISTRY.lock().remove(&key);
  let Some(shared) = shared else {
    return;
  };

  shared.fired.store(true, Ordering::Release);
  if shared.active.load(Ordering::Acquire) {
    shared.wake();
  }
}

#[derive(Debug)]
pub struct Sleep {
  deadline: Instant,
  state: SleepState,
}

#[derive(Debug)]
enum SleepState {
  Unregistered,
  Registered {
    timer_id: async_rt::TimerId,
    key: u64,
    shared: Arc<SleepShared>,
  },
  Done,
}

impl Sleep {
  fn new(deadline: Instant) -> Self {
    Self {
      deadline,
      state: SleepState::Unregistered,
    }
  }

  fn cancel(&mut self) {
    if let SleepState::Registered {
      timer_id,
      key,
      shared,
    } = &self.state
    {
      shared.active.store(false, Ordering::Release);

      // Remove our shared state first so even if the timer task was already queued
      // (e.g. due timers promoted into the macrotask queue), `on_timer_fire` becomes
      // a no-op and won't spuriously wake a dropped future.
      REGISTRY.lock().remove(key);

      let _ = async_rt::global().cancel_timer(*timer_id);
    }
    self.state = SleepState::Done;
  }

  fn is_ready(&self) -> bool {
    match &self.state {
      SleepState::Done => true,
      SleepState::Registered { shared, .. } => shared.fired.load(Ordering::Acquire),
      SleepState::Unregistered => false,
    }
  }
}

impl Future for Sleep {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.get_mut();

    if matches!(this.state, SleepState::Done) {
      return Poll::Ready(());
    }

    if async_rt::global().now() >= this.deadline {
      this.cancel();
      return Poll::Ready(());
    }

    if this.is_ready() {
      this.cancel();
      return Poll::Ready(());
    }

    match &mut this.state {
      SleepState::Unregistered => {
        let shared = Arc::new(SleepShared {
          active: AtomicBool::new(true),
          fired: AtomicBool::new(false),
          waker: GcAwareMutex::new(Some(cx.waker().clone())),
        });

        let key = NEXT_REGISTRATION_KEY.fetch_add(1, Ordering::Relaxed);
        REGISTRY.lock().insert(key, shared.clone());

        let data = key as usize as *mut u8;
        let timer_id = async_rt::global().schedule_timer(this.deadline, Task::new(on_timer_fire, data));
        this.state = SleepState::Registered { timer_id, key, shared };
        Poll::Pending
      }
      SleepState::Registered { shared, .. } => {
        shared.update_waker(cx.waker());
        Poll::Pending
      }
      SleepState::Done => Poll::Ready(()),
    }
  }
}

impl Drop for Sleep {
  fn drop(&mut self) {
    self.cancel();
  }
}

// -------------------------------------------------------------------------------------------------
// Timeout
// -------------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Timeout<F> {
  fut: F,
  sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
  type Output = Result<F::Output, TimeoutError>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    // Safety: We never move `fut` after the outer `Timeout` is pinned.
    let this = unsafe { self.get_unchecked_mut() };

    if let Poll::Ready(out) = unsafe { Pin::new_unchecked(&mut this.fut) }.poll(cx) {
      this.sleep.cancel();
      return Poll::Ready(Ok(out));
    }

    if Pin::new(&mut this.sleep).poll(cx).is_ready() {
      return Poll::Ready(Err(TimeoutError));
    }

    Poll::Pending
  }
}

// -------------------------------------------------------------------------------------------------
// Debug / test hooks
// -------------------------------------------------------------------------------------------------

/// Test-only hook: execute `f` while holding the sleep/timeout registry lock.
///
/// This exists to deterministically force contention on the time module's
/// internal registry lock for stop-the-world safepoint tests.
#[doc(hidden)]
pub fn debug_with_registry_lock<R>(f: impl FnOnce() -> R) -> R {
  let _guard = REGISTRY.lock();
  f()
}

/// Test-only helper: number of timer registrations currently owned by this module.
///
/// This is primarily used to assert that `Sleep`/`Timeout` correctly cancel their
/// registrations on drop/completion.
#[doc(hidden)]
pub fn debug_registration_count() -> usize {
  REGISTRY.lock().len()
}

#[doc(hidden)]
pub fn debug_clear_state_for_tests() {
  REGISTRY.lock().clear();
}

// -------------------------------------------------------------------------------------------------
// Async driver integration
// -------------------------------------------------------------------------------------------------

pub mod driver;

pub use crate::timer_wheel::TimerKey;
pub use driver::TimerDriver;
