use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::rt_async_poll_legacy as rt_async_poll;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::RawWaker;
use std::task::RawWakerVTable;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

fn noop_waker() -> Waker {
  unsafe fn clone(_: *const ()) -> RawWaker {
    RawWaker::new(std::ptr::null(), &VTABLE)
  }
  unsafe fn wake(_: *const ()) {}
  unsafe fn wake_by_ref(_: *const ()) {}
  unsafe fn drop_waker(_: *const ()) {}

  static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
  unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
  unsafe fn clone(data: *const ()) -> RawWaker {
    // Safety: the raw waker's data pointer is always an `Arc<AtomicUsize>`.
    let arc = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
    let cloned = arc.clone();
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
  }

  unsafe fn wake(data: *const ()) {
    let arc = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
    arc.fetch_add(1, Ordering::SeqCst);
    // Drop the Arc (consumes one ref).
  }

  unsafe fn wake_by_ref(data: *const ()) {
    let arc = Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize);
    arc.fetch_add(1, Ordering::SeqCst);
    // Don't consume the Arc's refcount.
    std::mem::forget(arc);
  }

  unsafe fn drop_waker(data: *const ()) {
    drop(Arc::<AtomicUsize>::from_raw(data as *const AtomicUsize));
  }

  static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
  unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(counter) as *const (), &VTABLE)) }
}

fn poll_once<F: Future>(mut fut: Pin<&mut F>, cx: &mut Context<'_>) -> Poll<F::Output> {
  fut.as_mut().poll(cx)
}

#[test]
fn sleep_zero_completes_immediately() {
  let _rt = TestRuntimeGuard::new();

  assert_eq!(runtime_native::async_rt::debug_timer_count(), 0);
  assert_eq!(runtime_native::time::debug_registration_count(), 0);

  let waker = noop_waker();
  let mut cx = Context::from_waker(&waker);

  let mut sleep = runtime_native::time::sleep(Duration::ZERO);
  let mut sleep = Pin::new(&mut sleep);
  assert!(poll_once(sleep.as_mut(), &mut cx).is_ready());

  assert_eq!(runtime_native::async_rt::debug_timer_count(), 0);
  assert_eq!(runtime_native::time::debug_registration_count(), 0);
}

#[test]
fn sleep_short_completes_after_time_advances() {
  let _rt = TestRuntimeGuard::new();

  let counter = Arc::new(AtomicUsize::new(0));
  let waker = counting_waker(counter.clone());
  let mut cx = Context::from_waker(&waker);

  let mut sleep = runtime_native::time::sleep(Duration::from_millis(20));
  let mut sleep = Pin::new(&mut sleep);

  assert!(poll_once(sleep.as_mut(), &mut cx).is_pending());
  assert_eq!(runtime_native::async_rt::debug_timer_count(), 1);
  assert_eq!(runtime_native::time::debug_registration_count(), 1);

  rt_async_poll();

  assert!(poll_once(sleep.as_mut(), &mut cx).is_ready());
  assert_eq!(runtime_native::async_rt::debug_timer_count(), 0);
  assert_eq!(runtime_native::time::debug_registration_count(), 0);
  assert!(counter.load(Ordering::SeqCst) > 0, "sleep did not wake its waker");
}

#[test]
fn timeout_cancels_timer_when_inner_future_completes_first() {
  let _rt = TestRuntimeGuard::new();

  let counter = Arc::new(AtomicUsize::new(0));
  let waker = counting_waker(counter);
  let mut cx = Context::from_waker(&waker);

  let fut = runtime_native::time::timeout(Duration::from_millis(50), async {
    runtime_native::time::sleep(Duration::from_millis(5)).await;
    123u32
  });
  let mut fut = Box::pin(fut);

  assert!(poll_once(fut.as_mut(), &mut cx).is_pending());
  assert_eq!(runtime_native::async_rt::debug_timer_count(), 2, "expected inner sleep + timeout timers");
  assert_eq!(
    runtime_native::time::debug_registration_count(),
    2,
    "expected inner sleep + timeout registrations"
  );

  let start = Instant::now();
  loop {
    match poll_once(fut.as_mut(), &mut cx) {
      Poll::Ready(res) => {
        assert_eq!(res.unwrap(), 123);
        break;
      }
      Poll::Pending => {
        assert!(
          start.elapsed() < Duration::from_secs(2),
          "timeout test took too long (possible timer leak)"
        );
        rt_async_poll();
      }
    }
  }

  assert_eq!(runtime_native::async_rt::debug_timer_count(), 0, "timeout should cancel its timer on success");
  assert_eq!(runtime_native::time::debug_registration_count(), 0);
}

#[test]
fn dropping_sleep_cancels_and_does_not_spuriously_wake() {
  let _rt = TestRuntimeGuard::new();

  let counter = Arc::new(AtomicUsize::new(0));
  let waker = counting_waker(counter.clone());
  let mut cx = Context::from_waker(&waker);

  let mut sleep = runtime_native::time::sleep(Duration::from_millis(50));
  {
    let mut sleep_pin = Pin::new(&mut sleep);
    assert!(poll_once(sleep_pin.as_mut(), &mut cx).is_pending());
  }

  assert_eq!(runtime_native::async_rt::debug_timer_count(), 1);
  assert_eq!(runtime_native::time::debug_registration_count(), 1);

  drop(sleep);

  assert_eq!(runtime_native::async_rt::debug_timer_count(), 0);
  assert_eq!(runtime_native::time::debug_registration_count(), 0);

  // Even if the timer was already promoted into a runnable task before cancellation,
  // the callback must become a no-op once the Sleep is dropped.
  rt_async_poll();
  assert_eq!(counter.load(Ordering::SeqCst), 0, "dropped Sleep must not wake");
}
