use crate::abi::{RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE};
use crate::async_rt::{self, WatcherId};
use crate::sync::GcAwareMutex;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

static NEXT_REGISTRY_KEY: AtomicUsize = AtomicUsize::new(1);
static REGISTRY: Lazy<GcAwareMutex<HashMap<usize, Arc<State>>>> =
  Lazy::new(|| GcAwareMutex::new(HashMap::new()));

extern "C" fn drop_registry_entry(data: *mut u8) {
  let key = data as usize;
  REGISTRY.lock().remove(&key);
}

/// A small, runtime-native reactor-backed wrapper for awaiting fd readiness.
///
/// This is a minimal `AsyncFd`-style API intended for internal use by `runtime-native`.
/// It registers interest with the process-global reactor and wakes the awaiting task
/// once readiness events arrive.
///
/// ## Nonblocking / edge-triggered contract
///
/// The wrapped file descriptor **must already be `O_NONBLOCK`**. `AsyncFd` does not
/// modify fd flags.
///
/// The fd must remain `O_NONBLOCK` for the lifetime of the registration. Clearing
/// `O_NONBLOCK` while the fd is registered violates the reactor contract and may
/// cause readiness polling to fail.
///
/// This is required because runtime-native's reactor uses **edge-triggered**
/// readiness notifications. If a blocking fd is registered, readiness wakeups may
/// stall (no new edges will be delivered while the consumer is blocked).
pub struct AsyncFd {
  fd: OwnedFd,
  state: Arc<State>,
}

impl AsyncFd {
  /// Create a new `AsyncFd` wrapper.
  ///
  /// The provided fd must already be set to `O_NONBLOCK`. If it isn't, the first
  /// attempt to await readability/writability will fail with
  /// [`io::ErrorKind::InvalidInput`].
  ///
  /// The fd must remain `O_NONBLOCK` for as long as it is registered with the
  /// reactor (i.e. for as long as there are pending `readable()`/`writable()`
  /// waiters).
  pub fn new(fd: OwnedFd) -> Self {
    let raw = fd.as_raw_fd();
    Self {
      fd,
      state: Arc::new(State::new(raw)),
    }
  }

  pub fn readable(&self) -> Readable<'_> {
    Readable::new(self)
  }

  pub fn writable(&self) -> Writable<'_> {
    Writable::new(self)
  }
}

impl AsRawFd for AsyncFd {
  fn as_raw_fd(&self) -> RawFd {
    self.fd.as_raw_fd()
  }
}

impl Drop for AsyncFd {
  fn drop(&mut self) {
    // Borrowed futures cannot outlive `AsyncFd`, but be defensive and force a
    // reactor deregistration if the watcher is still installed.
    let _ = self.state.force_deregister();
  }
}

pub struct Readable<'a> {
  fd: &'a AsyncFd,
  waiter_id: Option<u64>,
  start_gen: u64,
}

impl<'a> Readable<'a> {
  fn new(fd: &'a AsyncFd) -> Self {
    Self {
      fd,
      waiter_id: None,
      start_gen: 0,
    }
  }
}

impl Future for Readable<'_> {
  type Output = io::Result<()>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let mut inner = self
      .fd
      .state
      .inner
      .lock();

    let current = inner.readable_gen;
    if let Some(id) = self.waiter_id {
      if current != self.start_gen {
        return Poll::Ready(Ok(()));
      }
      inner.update_read_waker(id, cx.waker());
      return Poll::Pending;
    }

    let id = inner.next_waiter_id;
    inner.next_waiter_id = inner.next_waiter_id.wrapping_add(1);
    self.waiter_id = Some(id);
    self.start_gen = current;
    inner.readable_waiters.insert(id, cx.waker().clone());

    // Register interest once we have a waiter installed.
    if let Err(err) = self.fd.state.sync_watcher_locked(&mut inner, &self.fd.state) {
      inner.readable_waiters.remove(&id);
      self.waiter_id = None;
      return Poll::Ready(Err(err));
    }

    Poll::Pending
  }
}

impl Drop for Readable<'_> {
  fn drop(&mut self) {
    let Some(id) = self.waiter_id.take() else {
      return;
    };

    let mut inner = self
      .fd
      .state
      .inner
      .lock();
    inner.readable_waiters.remove(&id);
    let _ = self.fd.state.sync_watcher_locked(&mut inner, &self.fd.state);
  }
}

pub struct Writable<'a> {
  fd: &'a AsyncFd,
  waiter_id: Option<u64>,
  start_gen: u64,
}

impl<'a> Writable<'a> {
  fn new(fd: &'a AsyncFd) -> Self {
    Self {
      fd,
      waiter_id: None,
      start_gen: 0,
    }
  }
}

impl Future for Writable<'_> {
  type Output = io::Result<()>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let mut inner = self
      .fd
      .state
      .inner
      .lock();

    let current = inner.writable_gen;
    if let Some(id) = self.waiter_id {
      if current != self.start_gen {
        return Poll::Ready(Ok(()));
      }
      inner.update_write_waker(id, cx.waker());
      return Poll::Pending;
    }

    let id = inner.next_waiter_id;
    inner.next_waiter_id = inner.next_waiter_id.wrapping_add(1);
    self.waiter_id = Some(id);
    self.start_gen = current;
    inner.writable_waiters.insert(id, cx.waker().clone());

    if let Err(err) = self.fd.state.sync_watcher_locked(&mut inner, &self.fd.state) {
      inner.writable_waiters.remove(&id);
      self.waiter_id = None;
      return Poll::Ready(Err(err));
    }

    Poll::Pending
  }
}

impl Drop for Writable<'_> {
  fn drop(&mut self) {
    let Some(id) = self.waiter_id.take() else {
      return;
    };

    let mut inner = self
      .fd
      .state
      .inner
      .lock();
    inner.writable_waiters.remove(&id);
    let _ = self.fd.state.sync_watcher_locked(&mut inner, &self.fd.state);
  }
}

struct State {
  fd: RawFd,
  inner: GcAwareMutex<StateInner>,
}

struct StateInner {
  watcher_id: Option<WatcherId>,
  watcher_key: Option<usize>,
  interests: u32,

  next_waiter_id: u64,
  readable_gen: u64,
  writable_gen: u64,
  readable_waiters: HashMap<u64, Waker>,
  writable_waiters: HashMap<u64, Waker>,
}

impl State {
  fn new(fd: RawFd) -> Self {
    Self {
      fd,
      inner: GcAwareMutex::new(StateInner {
        watcher_id: None,
        watcher_key: None,
        interests: 0,
        next_waiter_id: 1,
        readable_gen: 0,
        writable_gen: 0,
        readable_waiters: HashMap::new(),
        writable_waiters: HashMap::new(),
      }),
    }
  }

  fn sync_watcher_locked(&self, inner: &mut StateInner, arc: &Arc<State>) -> io::Result<()> {
    let mut desired = 0;
    if !inner.readable_waiters.is_empty() {
      desired |= RT_IO_READABLE;
    }
    if !inner.writable_waiters.is_empty() {
      desired |= RT_IO_WRITABLE;
    }

    if desired == 0 {
      if let Some(id) = inner.watcher_id.take() {
        let key = inner
          .watcher_key
          .take()
          .expect("io watcher registered without watcher_key");
        inner.interests = 0;
        let _ = async_rt::global().deregister_fd(id);
        REGISTRY.lock().remove(&key);
      }
      return Ok(());
    }

    if let Some(id) = inner.watcher_id {
      if inner.interests != desired {
        if !async_rt::global().update_io(id, desired) {
          // If we fail to update the reactor registration, drop the existing
          // watcher so we don't leave a stale fd registration behind.
          let _ = async_rt::global().deregister_fd(id);
          let key = inner
            .watcher_key
            .take()
            .expect("io watcher registered without watcher_key");
          inner.watcher_id = None;
          inner.interests = 0;
          REGISTRY.lock().remove(&key);
          return Err(io::Error::new(
            io::ErrorKind::Other,
            "failed to update reactor interest",
          ));
        }
        inner.interests = desired;
      }
      return Ok(());
    }

    crate::rt_ensure_init();
    let key = NEXT_REGISTRY_KEY.fetch_add(1, Ordering::Relaxed);
    REGISTRY.lock().insert(key, Arc::clone(arc));
    let id = match async_rt::global().register_io_with_drop(
      self.fd,
      desired,
      on_io_ready,
      key as *mut u8,
      drop_registry_entry,
    ) {
      Ok(id) => id,
      Err(e) => {
        REGISTRY.lock().remove(&key);
        return Err(e);
      }
    };

    inner.watcher_id = Some(id);
    inner.watcher_key = Some(key);
    inner.interests = desired;
    Ok(())
  }

  fn force_deregister(&self) {
    let mut inner = self.inner.lock();
    if let Some(id) = inner.watcher_id.take() {
      let key = inner.watcher_key.take().expect("watcher_id without watcher_key");
      inner.interests = 0;
      inner.readable_waiters.clear();
      inner.writable_waiters.clear();
      let _ = async_rt::global().deregister_fd(id);
      REGISTRY.lock().remove(&key);
    }
  }
}

extern "C" fn on_io_ready(events: u32, data: *mut u8) {
  let key = data as usize;
  let state = {
    let reg = REGISTRY.lock();
    reg.get(&key).cloned()
  };
  let Some(state) = state else {
    return;
  };

  let mut wake: Vec<Waker> = Vec::new();
  {
    let mut inner = state.inner.lock();
    if events & (RT_IO_READABLE | RT_IO_ERROR) != 0 {
      inner.readable_gen = inner.readable_gen.wrapping_add(1);
      wake.extend(inner.readable_waiters.values().cloned());
    }
    if events & (RT_IO_WRITABLE | RT_IO_ERROR) != 0 {
      inner.writable_gen = inner.writable_gen.wrapping_add(1);
      wake.extend(inner.writable_waiters.values().cloned());
    }
  }

  for waker in wake {
    waker.wake();
  }
}

impl StateInner {
  fn update_read_waker(&mut self, id: u64, waker: &Waker) {
    let Some(existing) = self.readable_waiters.get_mut(&id) else {
      self.readable_waiters.insert(id, waker.clone());
      return;
    };
    if !existing.will_wake(waker) {
      *existing = waker.clone();
    }
  }

  fn update_write_waker(&mut self, id: u64, waker: &Waker) {
    let Some(existing) = self.writable_waiters.get_mut(&id) else {
      self.writable_waiters.insert(id, waker.clone());
      return;
    };
    if !existing.will_wake(waker) {
      *existing = waker.clone();
    }
  }
}
