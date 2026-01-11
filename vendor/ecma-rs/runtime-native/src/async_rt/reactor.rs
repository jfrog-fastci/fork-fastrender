use crate::abi::{RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE};
use crate::async_rt::{Task, TaskDropFn};
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static IN_REACTOR_WAIT: AtomicBool = AtomicBool::new(false);

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

/// Test-only signal indicating whether some thread is currently blocked in the reactor syscall
/// (`epoll_wait` / `kevent`).
///
/// This is not a synchronization primitive; it only exists to make it possible to deterministically
/// reproduce and test the "GC request while blocked in the reactor wait syscall" scenario.
pub(crate) fn debug_in_epoll_wait() -> bool {
  IN_REACTOR_WAIT.load(Ordering::Relaxed)
}

bitflags! {
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct Interest: u32 {
    const READABLE = 0b01;
    const WRITABLE = 0b10;
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

const WAKE_TOKEN: u64 = 0;

pub struct Reactor {
  sys: sys::ReactorSys,
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
    Ok(Self {
      sys: sys::ReactorSys::new()?,
      next_id: AtomicU64::new(1),
      watchers_count: AtomicUsize::new(0),
      watchers: Mutex::new(HashMap::new()),
    })
  }

  pub fn wake(&self) {
    self.sys.wake();
  }

  pub fn drain_wake(&self) -> io::Result<()> {
    self.sys.drain_wake()
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

    let id = WatcherId(self.next_id.fetch_add(1, Ordering::Relaxed));
    self.sys.ctl_add(fd, interest, id.as_raw())?;
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

    let id = WatcherId(self.next_id.fetch_add(1, Ordering::Relaxed));
    self.sys.ctl_add(fd, interest, id.as_raw())?;
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

    if self.sys.ctl_mod(watcher.fd, interest, id.as_raw()).is_err() {
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
        let _ = self.sys.ctl_del(w.fd);
      }
      watcher
    };
    let Some(watcher) = watcher else { return false };
    if let WatcherKind::Io(io) = watcher.kind {
      io.active.store(false, Ordering::Release);
      if let Some(drop) = io.drop {
        drop(io.data);
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
        let _ = self.sys.ctl_del(watcher.fd);
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
    let (ready_tokens, needs_wake_drain) = self.sys.wait(timeout_ms)?;

    if needs_wake_drain {
      self.sys.drain_wake()?;
    }

    if ready_tokens.is_empty() {
      return Ok(Vec::new());
    }

    let watchers = self.watchers.lock().unwrap();
    let mut tasks = Vec::with_capacity(ready_tokens.len());
    for (id, events) in ready_tokens {
      if let Some(watcher) = watchers.get(&id) {
        match &watcher.kind {
          WatcherKind::Task(task) => {
            let _ = watcher.interest; // Placeholder for future event filtering.
            tasks.push(task.clone());
          }
          WatcherKind::Io(io) => {
            let delivered = events & (io.interests | RT_IO_ERROR);
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
    }
    Ok(tasks)
  }
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

#[cfg(target_os = "linux")]
mod sys {
  use super::{Interest, WatcherId, IN_REACTOR_WAIT, WAKE_TOKEN};
  use crate::abi::{RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE};
  use crate::platform::linux_epoll::{Epoll, EventFd};
  use crate::threading;
  use std::io;
  use std::os::fd::RawFd;
  use std::sync::atomic::Ordering;

  pub(super) struct ReactorSys {
    epoll: Epoll,
    wake: EventFd,
  }

  impl ReactorSys {
    pub(super) fn new() -> io::Result<Self> {
      let epoll = Epoll::new()?;
      let wake = EventFd::new()?;
      epoll.ctl_add(wake.as_raw_fd(), libc::EPOLLIN as u32, WAKE_TOKEN)?;
      Ok(Self { epoll, wake })
    }

    pub(super) fn wake(&self) {
      self.wake.wake();
    }

    pub(super) fn drain_wake(&self) -> io::Result<()> {
      self.wake.drain()
    }

    pub(super) fn ctl_add(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
      self.epoll.ctl_add(fd, interest_to_epoll_events(interest), token)
    }

    pub(super) fn ctl_mod(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
      self.epoll.ctl_mod(fd, interest_to_epoll_events(interest), token)
    }

    pub(super) fn ctl_del(&self, fd: RawFd) -> io::Result<()> {
      self.epoll.ctl_del(fd)
    }

    pub(super) fn wait(&self, timeout_ms: i32) -> io::Result<(Vec<(WatcherId, u32)>, bool)> {
      const MAX_EVENTS: usize = 64;
      let mut events: [libc::epoll_event; MAX_EVENTS] = unsafe { std::mem::zeroed() };
      let n = if timeout_ms == 0 {
        self.epoll.wait(&mut events, timeout_ms)?
      } else {
        IN_REACTOR_WAIT.store(true, Ordering::Release);
        let guard = threading::ParkedGuard::new();
        let res = self.epoll.wait(&mut events, timeout_ms);
        // Clear this debug flag before potentially blocking while un-parking.
        IN_REACTOR_WAIT.store(false, Ordering::Release);
        drop(guard);
        res?
      };

      if n == 0 {
        return Ok((Vec::new(), false));
      }
      crate::rt_trace::epoll_wakeups_inc();

      let mut needs_wake_drain = false;
      let mut ready_tokens: Vec<(WatcherId, u32)> = Vec::new();
      for ev in events.iter().take(n) {
        let token = ev.u64;
        if token == WAKE_TOKEN {
          needs_wake_drain = true;
        } else {
          ready_tokens.push((WatcherId::from_raw(token), epoll_events_to_rt(ev.events)));
        }
      }

      Ok((ready_tokens, needs_wake_drain))
    }
  }

  fn interest_to_epoll_events(interest: Interest) -> u32 {
    let mut events = 0;
    if interest.contains(Interest::READABLE) {
      events |= libc::EPOLLIN as u32;
    }
    if interest.contains(Interest::WRITABLE) {
      events |= libc::EPOLLOUT as u32;
    }
    // `async_rt` readiness watchers follow the runtime-native reactor contract: edge-triggered.
    // Ensure all registrations include EPOLLET so consumers must drain reads/writes until EAGAIN.
    //
    // Always report error/hangup conditions to the callback.
    events | (libc::EPOLLET | libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLRDHUP) as u32
  }

  fn epoll_events_to_rt(events: u32) -> u32 {
    let mut rt = 0;
    if events & (libc::EPOLLIN as u32) != 0 {
      rt |= RT_IO_READABLE;
    }
    if events & (libc::EPOLLOUT as u32) != 0 {
      rt |= RT_IO_WRITABLE;
    }
    if events & ((libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLRDHUP) as u32) != 0 {
      rt |= RT_IO_ERROR;
    }
    rt
  }
}

#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]
mod sys {
  use super::{Interest, WatcherId, IN_REACTOR_WAIT, WAKE_TOKEN};
  use crate::abi::{RT_IO_ERROR, RT_IO_READABLE, RT_IO_WRITABLE};
  use crate::platform::kqueue::{kevent_token, make_kevent, Kqueue, Waker};
  use crate::threading;
  use std::io;
  use std::os::fd::RawFd;
  use std::sync::atomic::Ordering;

  pub(super) struct ReactorSys {
    kqueue: Kqueue,
    wake: Waker,
  }

  impl ReactorSys {
    pub(super) fn new() -> io::Result<Self> {
      let kqueue = Kqueue::new()?;
      let wake = Waker::new(&kqueue, WAKE_TOKEN)?;
      Ok(Self { kqueue, wake })
    }

    pub(super) fn wake(&self) {
      self.wake.wake();
    }

    pub(super) fn drain_wake(&self) -> io::Result<()> {
      self.wake.drain()
    }

    pub(super) fn ctl_add(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
      let mut changes: Vec<libc::kevent> = Vec::new();
      if interest.contains(Interest::READABLE) {
        changes.push(make_kevent(
          fd,
          libc::EVFILT_READ,
          token,
          libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        ));
      }
      if interest.contains(Interest::WRITABLE) {
        changes.push(make_kevent(
          fd,
          libc::EVFILT_WRITE,
          token,
          libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        ));
      }
      self.kqueue.ctl(&changes)
    }

    pub(super) fn ctl_mod(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
      self.ctl_del(fd)?;
      self.ctl_add(fd, interest, token)
    }

    pub(super) fn ctl_del(&self, fd: RawFd) -> io::Result<()> {
      // Best-effort delete; ignore ENOENT (already removed).
      for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
        let kev = libc::kevent {
          ident: fd as libc::uintptr_t,
          filter,
          flags: libc::EV_DELETE,
          fflags: 0,
          data: 0,
          udata: std::ptr::null_mut(),
        };

        loop {
          let rc = unsafe {
            libc::kevent(
              self.kqueue.as_raw_fd(),
              &kev as *const libc::kevent,
              1,
              std::ptr::null_mut(),
              0,
              std::ptr::null(),
            )
          };
          if rc >= 0 {
            break;
          }

          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          if err.raw_os_error() == Some(libc::ENOENT) {
            break;
          }
          return Err(err);
        }
      }
      Ok(())
    }

    pub(super) fn wait(&self, timeout_ms: i32) -> io::Result<(Vec<(WatcherId, u32)>, bool)> {
      const MAX_EVENTS: usize = 64;
      let mut events = [libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
      }; MAX_EVENTS];

      let n = if timeout_ms == 0 {
        self.kqueue.wait(&mut events, timeout_ms)?
      } else {
        IN_REACTOR_WAIT.store(true, Ordering::Release);
        let guard = threading::ParkedGuard::new();
        let res = self.kqueue.wait(&mut events, timeout_ms);
        IN_REACTOR_WAIT.store(false, Ordering::Release);
        drop(guard);
        res?
      };

      if n == 0 {
        return Ok((Vec::new(), false));
      }
      crate::rt_trace::epoll_wakeups_inc();

      let mut needs_wake_drain = false;
      let mut ready_tokens: Vec<(WatcherId, u32)> = Vec::new();

      for kev in events.iter().take(n) {
        let token = kevent_token(kev);
        if token == WAKE_TOKEN {
          needs_wake_drain = true;
          continue;
        }

        let ready = kevent_to_rt(kev);
        if ready == 0 {
          continue;
        }

        if let Some((_id, existing)) = ready_tokens.iter_mut().find(|(id, _)| id.as_raw() == token) {
          *existing |= ready;
        } else {
          ready_tokens.push((WatcherId::from_raw(token), ready));
        }
      }

      Ok((ready_tokens, needs_wake_drain))
    }
  }

  fn kevent_to_rt(kev: &libc::kevent) -> u32 {
    let mut rt = 0;
    if kev.filter == libc::EVFILT_READ {
      rt |= RT_IO_READABLE;
    }
    if kev.filter == libc::EVFILT_WRITE {
      rt |= RT_IO_WRITABLE;
    }
    if (kev.flags & (libc::EV_ERROR | libc::EV_EOF)) != 0 {
      rt |= RT_IO_ERROR;
    }
    rt
  }
}

#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
)))]
mod sys {
  use super::{Interest, WatcherId};
  use std::io;
  use std::os::fd::RawFd;

  pub(super) struct ReactorSys;

  impl ReactorSys {
    pub(super) fn new() -> io::Result<Self> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "runtime-native async reactor is only supported on Linux (epoll) and kqueue platforms",
      ))
    }

    pub(super) fn wake(&self) {}

    pub(super) fn drain_wake(&self) -> io::Result<()> {
      Ok(())
    }

    pub(super) fn ctl_add(&self, _fd: RawFd, _interest: Interest, _token: u64) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn ctl_mod(&self, _fd: RawFd, _interest: Interest, _token: u64) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn ctl_del(&self, _fd: RawFd) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn wait(&self, _timeout_ms: i32) -> io::Result<(Vec<(WatcherId, u32)>, bool)> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }
  }
}
