use crate::platform::linux_epoll::EventFd;
use crate::sync::GcAwareMutex;
use crate::threading;
use io_uring::opcode;
use io_uring::types;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Once;
use std::sync::OnceLock;

static IN_URING_WAIT: AtomicBool = AtomicBool::new(false);

#[doc(hidden)]
pub fn debug_in_uring_wait() -> bool {
  IN_URING_WAIT.load(Ordering::Relaxed)
}

static SAFEPOINT_WAKER_ONCE: Once = Once::new();
static SAFEPOINT_WAKERS: OnceLock<GcAwareMutex<Vec<Arc<EventFd>>>> = OnceLock::new();

fn safepoint_wakers() -> &'static GcAwareMutex<Vec<Arc<EventFd>>> {
  SAFEPOINT_WAKERS.get_or_init(|| GcAwareMutex::new(Vec::new()))
}

fn wake_all_safepoint_wakers() {
  // Called from the safepoint coordinator while the global GC epoch may be odd; use the GC-safe
  // locking path to avoid spinning on epoch checks.
  let wakers = { safepoint_wakers().lock_for_gc().clone() };
  for w in wakers {
    w.wake();
  }
}

struct SafepointWakeRegistration {
  wake: Arc<EventFd>,
}

impl Drop for SafepointWakeRegistration {
  fn drop(&mut self) {
    let mut wakers = safepoint_wakers().lock();
    wakers.retain(|w| !Arc::ptr_eq(w, &self.wake));
  }
}

const WAKE_TOKEN: u64 = 1;

#[derive(Clone)]
pub struct IoUringWaker {
  wake: Arc<EventFd>,
}

impl IoUringWaker {
  pub fn wake(&self) {
    self.wake.wake();
  }
}

pub struct IoUringCqeWaiter {
  ring: io_uring::IoUring,
  wake: Arc<EventFd>,
  _safepoint_waker_reg: SafepointWakeRegistration,
}

impl IoUringCqeWaiter {
  pub fn new() -> io::Result<Self> {
    SAFEPOINT_WAKER_ONCE.call_once(|| {
      threading::register_reactor_waker(wake_all_safepoint_wakers);
    });

    let wake = Arc::new(EventFd::new()?);
    {
      let mut wakers = safepoint_wakers().lock();
      wakers.push(wake.clone());
    }
    let reg = SafepointWakeRegistration { wake: wake.clone() };

    let ring = io_uring::IoUring::new(8)?;

    let mut waiter = Self {
      ring,
      wake,
      _safepoint_waker_reg: reg,
    };
    waiter.arm_wake_poll()?;
    Ok(waiter)
  }

  pub fn waker(&self) -> IoUringWaker {
    IoUringWaker {
      wake: self.wake.clone(),
    }
  }

  pub fn wake(&self) {
    self.wake.wake();
  }

  pub fn wait(&mut self) -> io::Result<()> {
    struct WaitGuard;
    impl Drop for WaitGuard {
      fn drop(&mut self) {
        threading::set_parked(false);
        IN_URING_WAIT.store(false, Ordering::Release);
      }
    }

    loop {
      threading::safepoint_poll();
      IN_URING_WAIT.store(true, Ordering::Release);
      threading::set_parked(true);
      let guard = WaitGuard;

      let res = self.ring.submit_and_wait(1);
      drop(guard);

      match res {
        Ok(_) => break,
        Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
        Err(err) => return Err(err),
      }
    }

    threading::safepoint_poll();
    self.drain_and_rearm()?;
    Ok(())
  }

  fn arm_wake_poll(&mut self) -> io::Result<()> {
    let sqe = opcode::PollAdd::new(types::Fd(self.wake.as_raw_fd()), libc::POLLIN as u32)
      .build()
      .user_data(WAKE_TOKEN);

    // SAFETY: `sqe` is a valid submission entry; `io_uring` takes a copy.
    unsafe {
      self
        .ring
        .submission()
        .push(&sqe)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full"))?;
    }
    self.ring.submit()?;
    Ok(())
  }

  fn drain_and_rearm(&mut self) -> io::Result<()> {
    let mut needs_rearm = false;
    for cqe in self.ring.completion() {
      if cqe.user_data() == WAKE_TOKEN {
        needs_rearm = true;
      }
    }

    if needs_rearm {
      self.wake.drain()?;
      self.arm_wake_poll()?;
    }

    Ok(())
  }
}
