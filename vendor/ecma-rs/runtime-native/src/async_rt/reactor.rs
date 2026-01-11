use crate::abi::{RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE};
use crate::async_rt::{Task, TaskDropFn};
use crate::reactor::{
  Event as ReactorEvent, Interest as ReactorInterest, Reactor as SysReactor, Token as ReactorToken,
  Waker,
};
use crate::threading;
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static IN_EPOLL_WAIT: AtomicBool = AtomicBool::new(false);

fn ensure_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags != -1 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };
  if flags & libc::O_NONBLOCK == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "reactor requires all registered file descriptors to be O_NONBLOCK (edge-triggered contract)",
    ));
  }
  Ok(())
}

/// Test-only signal indicating whether some thread is currently blocked in the reactor wait syscall
/// (`epoll_wait` on Linux, `kevent` on kqueue platforms).
///
/// This is not a synchronization primitive; it only exists to make it possible to deterministically
/// reproduce and test the "GC request while blocked in the reactor wait" scenario.
pub(crate) fn debug_in_epoll_wait() -> bool {
  IN_EPOLL_WAIT.load(Ordering::Relaxed)
}

bitflags! {
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct Interest: u32 {
    const READABLE = 0b01;
    const WRITABLE = 0b10;
  }
}

impl Interest {
  fn to_reactor_interest(self) -> ReactorInterest {
    let mut out = ReactorInterest::empty();
    if self.contains(Self::READABLE) {
      out = out | ReactorInterest::READABLE;
    }
    if self.contains(Self::WRITABLE) {
      out = out | ReactorInterest::WRITABLE;
    }
    out
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WatcherId(u64);

impl WatcherId {
  pub fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  pub fn as_raw(self) -> u64 {
    self.0
  }
}

pub struct Reactor {
  sys: SysReactor,
  waker: Waker,

  next_id: AtomicU64,
  watchers_count: AtomicUsize,
  watchers: Mutex<HashMap<WatcherId, Watcher>>,
}

struct Watcher {
  fd: RawFd,
  interest: Interest,
  kind: WatcherKind,
}

enum WatcherKind {
  Task(Task),
  Io(IoWatcher),
}

struct IoWatcher {
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
  drop: Option<TaskDropFn>,
  active: Arc<AtomicBool>,
}

// Safety: opaque pointers are never dereferenced by the reactor. They are passed back to the
// callback on the single-threaded event loop.
unsafe impl Send for IoWatcher {}

struct IoTask {
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
  events: u32,
  active: Arc<AtomicBool>,
}

extern "C" fn run_io_task(data: *mut u8) {
  // Safety: `data` is allocated via `Box::into_raw(IoTask)` in `wait` and freed by the task drop
  // hook.
  let task = unsafe { &*(data as *const IoTask) };
  if task.active.load(Ordering::Acquire) {
    (task.cb)(task.events, task.data);
  }
}

extern "C" fn drop_io_task(data: *mut u8) {
  // Safety: allocated by `Box::into_raw` in `wait`.
  unsafe {
    drop(Box::from_raw(data as *mut IoTask));
  }
}

impl Reactor {
  pub fn new() -> io::Result<Self> {
    let sys = SysReactor::new()?;
    let waker = sys.waker();

    Ok(Self {
      sys,
      waker,
      next_id: AtomicU64::new(1),
      watchers_count: AtomicUsize::new(0),
      watchers: Mutex::new(HashMap::new()),
    })
  }

  pub fn wake(&self) {
    let _ = self.waker.wake();
  }

  pub fn drain_wake(&self) -> io::Result<()> {
    // The unified reactor drains the waker as part of `poll()`. There's no separate "drain"
    // primitive, so we use a zero-timeout poll to clear any pending wake edge.
    let mut events = Vec::new();
    let _ = self.sys.poll(&mut events, Some(Duration::from_millis(0)))?;
    Ok(())
  }

  pub fn has_watchers(&self) -> bool {
    self.watchers_count.load(Ordering::Acquire) > 0
  }

  pub fn register(&self, fd: RawFd, interest: Interest, task: Task) -> io::Result<WatcherId> {
    if interest.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reactor interest must include READABLE and/or WRITABLE",
      ));
    }
    ensure_nonblocking(fd)?;
    let mut watchers = self.watchers.lock().unwrap();
    if watchers.values().any(|w| w.fd == fd) {
      return Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "reactor fd is already registered (use update_io/deregister_fd instead of registering twice)",
      ));
    }
    let id = self.alloc_watcher_id()?;
    self.sys.register(
      unsafe { BorrowedFd::borrow_raw(fd) },
      watcher_id_to_token(id),
      interest.to_reactor_interest(),
    )?;
    watchers.insert(
      id,
      Watcher {
        fd,
        interest,
        kind: WatcherKind::Task(task),
      },
    );
    self.watchers_count.fetch_add(1, Ordering::Release);
    drop(watchers);
    self.wake();
    Ok(id)
  }

  /// Register an fd for RT_IO_* readiness notifications.
  ///
  /// Note: this is backed by an **edge-triggered** reactor. Callbacks must drain the fd (read/write
  /// until `WouldBlock`) before returning, otherwise no further readiness notifications may be
  /// delivered.
  pub fn register_io(
    &self,
    fd: RawFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
  ) -> io::Result<WatcherId> {
    self.register_io_with_drop(fd, interests, cb, data, None)
  }

  pub fn register_io_with_drop(
    &self,
    fd: RawFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
    drop: Option<TaskDropFn>,
  ) -> io::Result<WatcherId> {
    let interest = rt_interests_to_interest(interests);
    if interest.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reactor interest must include RT_IO_READABLE and/or RT_IO_WRITABLE",
      ));
    }
    ensure_nonblocking(fd)?;
    let mut watchers = self.watchers.lock().unwrap();
    if watchers.values().any(|w| w.fd == fd) {
      return Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "reactor fd is already registered (use rt_io_update/rt_io_unregister instead of registering twice)",
      ));
    }
    let id = self.alloc_watcher_id()?;
    self.sys.register(
      unsafe { BorrowedFd::borrow_raw(fd) },
      watcher_id_to_token(id),
      interest.to_reactor_interest(),
    )?;
    watchers.insert(
      id,
      Watcher {
        fd,
        interest,
        kind: WatcherKind::Io(IoWatcher {
          interests,
          cb,
          data,
          drop,
          active: Arc::new(AtomicBool::new(true)),
        }),
      },
    );
    self.watchers_count.fetch_add(1, Ordering::Release);
    std::mem::drop(watchers);
    self.wake();
    Ok(id)
  }

  pub fn update_io(&self, id: WatcherId, interests: u32) -> bool {
    let interest = rt_interests_to_interest(interests);
    if interest.is_empty() {
      return false;
    }
    let mut watchers = self.watchers.lock().unwrap();
    let Some(watcher) = watchers.get_mut(&id) else {
      return false;
    };

    // Ensure the fd still satisfies the nonblocking/edge-triggered contract.
    if ensure_nonblocking(watcher.fd).is_err() {
      return false;
    }

    if self
      .sys
      .reregister(
        unsafe { BorrowedFd::borrow_raw(watcher.fd) },
        watcher_id_to_token(id),
        interest.to_reactor_interest(),
      )
      .is_err()
    {
      return false;
    }

    watcher.interest = interest;
    if let WatcherKind::Io(io) = &mut watcher.kind {
      io.interests = interests;
    }
    drop(watchers);

    self.wake();
    true
  }

  pub fn deregister(&self, id: WatcherId) -> bool {
    let watcher = {
      let mut watchers = self.watchers.lock().unwrap();
      let watcher = watchers.remove(&id);
      if let Some(w) = &watcher {
        self.watchers_count.fetch_sub(1, Ordering::Release);
        // Ensure we remove the OS registration before releasing the lock so callers cannot race a
        // deregister+register on the same fd and accidentally delete the new registration.
        //
        // SAFETY: `fd` was previously registered and must remain open until `deregister`.
        let _ = self.sys.deregister(unsafe { BorrowedFd::borrow_raw(w.fd) });
      }
      watcher
    };
    let Some(watcher) = watcher else { return false };
    if let WatcherKind::Io(io) = watcher.kind {
      io.active.store(false, Ordering::Release);
      if let Some(drop) = io.drop {
        // Defer dropping callback state until the event loop observes the deregistration. This
        // avoids freeing `data` while an already-queued readiness task is still executing.
        struct DropCtx {
          drop: TaskDropFn,
          data: *mut u8,
        }

        extern "C" fn noop_drop_task(_data: *mut u8) {}

        extern "C" fn drop_drop_task(data: *mut u8) {
          // Safety: allocated by `Box::into_raw` in `deregister`.
          unsafe {
            let ctx = Box::from_raw(data as *mut DropCtx);
            (ctx.drop)(ctx.data);
          }
        }

        let ctx = Box::new(DropCtx { drop, data: io.data });
        super::global().enqueue_microtask(Task::new_with_drop(
          noop_drop_task,
          Box::into_raw(ctx) as *mut u8,
          drop_drop_task,
        ));
      }
    }
    self.wake();
    true
  }

  pub fn clear_watchers(&self) {
    // Don't hold the map lock while invoking watcher drop hooks; those hooks may queue work or call
    // back into the async runtime.
    let drained: Vec<(WatcherId, Watcher)> = {
      let mut watchers = self.watchers.lock().unwrap();
      if watchers.is_empty() {
        return;
      }
      // Update the count while still holding the lock so we don't race with concurrent
      // register/unregister calls.
      self.watchers_count.store(0, Ordering::Release);
      let drained: Vec<(WatcherId, Watcher)> = watchers.drain().collect();
      // Remove OS registrations while still holding the lock so future register calls cannot race a
      // delete-after-add ordering on the same fd.
      for (_id, watcher) in &drained {
        // SAFETY: `fd` was previously registered and must remain open until teardown.
        let _ = self.sys.deregister(unsafe { BorrowedFd::borrow_raw(watcher.fd) });
      }
      drained
    };

    for (_id, watcher) in drained {
      if let WatcherKind::Io(io) = &watcher.kind {
        io.active.store(false, Ordering::Release);
        if let Some(drop) = io.drop {
          drop(io.data);
        }
      }
    }
    self.wake();
  }

  pub fn wait(&self, timeout_ms: i32) -> io::Result<Vec<Task>> {
    debug_assert!(timeout_ms >= -1);

    let timeout = match timeout_ms {
      -1 => None,
      0 => Some(Duration::from_millis(0)),
      n => Some(Duration::from_millis(n as u64)),
    };

    let mut events: Vec<ReactorEvent> = Vec::with_capacity(64);

    let n = if timeout_ms == 0 {
      self.sys.poll(&mut events, timeout)?
    } else {
      IN_EPOLL_WAIT.store(true, Ordering::Release);
      let guard = threading::ParkedGuard::new();
      let res = self.sys.poll(&mut events, timeout);
      // Clear this debug flag before potentially blocking while un-parking.
      IN_EPOLL_WAIT.store(false, Ordering::Release);
      drop(guard);
      res?
    };

    if n == 0 {
      return Ok(Vec::new());
    }
    crate::rt_trace::epoll_wakeups_inc();

    let watchers = self.watchers.lock().unwrap();
    let mut tasks = Vec::with_capacity(n);

    for ev in events {
      if ev.token == ReactorToken::WAKE {
        continue;
      }

      let id = token_to_watcher_id(ev.token);
      let Some(watcher) = watchers.get(&id) else {
        continue;
      };

      match &watcher.kind {
        WatcherKind::Task(task) => {
          let _ = watcher.interest; // Placeholder for future event filtering.
          tasks.push(task.clone());
        }
        WatcherKind::Io(io) => {
          let ready = reactor_event_to_rt(&ev);
          let delivered = ready & (io.interests | RT_IO_ERROR);
          if delivered == 0 {
            continue;
          }
          let task = Box::new(IoTask {
            cb: io.cb,
            data: io.data,
            events: delivered,
            active: io.active.clone(),
          });
          tasks.push(Task::new_with_drop(
            run_io_task,
            Box::into_raw(task) as *mut u8,
            drop_io_task,
          ));
        }
      }
    }

    Ok(tasks)
  }

  fn alloc_watcher_id(&self) -> io::Result<WatcherId> {
    // `runtime_native::reactor::Token::WAKE` is `usize::MAX`, so we reserve that value and only hand
    // out IDs up to `usize::MAX - 1`. This also ensures the u64 -> usize token conversion is
    // lossless on 32-bit platforms.
    const MAX_ID: u64 = (usize::MAX - 1) as u64;

    self
      .next_id
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        if cur == 0 || cur > MAX_ID {
          return None;
        }
        Some(cur + 1)
      })
      .map(WatcherId)
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "watcher id space exhausted"))
  }
}

fn watcher_id_to_token(id: WatcherId) -> ReactorToken {
  ReactorToken(id.0 as usize)
}

fn token_to_watcher_id(token: ReactorToken) -> WatcherId {
  WatcherId(token.0 as u64)
}

fn rt_interests_to_interest(interests: u32) -> Interest {
  let mut out = Interest::empty();
  if interests & RT_IO_READABLE != 0 {
    out |= Interest::READABLE;
  }
  if interests & RT_IO_WRITABLE != 0 {
    out |= Interest::WRITABLE;
  }
  out
}

fn reactor_event_to_rt(ev: &ReactorEvent) -> u32 {
  let mut rt = 0;
  if ev.readable {
    rt |= RT_IO_READABLE;
  }
  if ev.writable {
    rt |= RT_IO_WRITABLE;
  }
  if ev.error || ev.read_closed || ev.write_closed {
    rt |= RT_IO_ERROR;
  }
  rt
}
