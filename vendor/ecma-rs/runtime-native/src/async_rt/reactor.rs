use crate::abi::RT_IO_ERROR;
use crate::abi::RT_IO_READABLE;
use crate::abi::RT_IO_WRITABLE;
use crate::async_rt::Task;
use crate::platform::linux_epoll::Epoll;
use crate::platform::linux_epoll::EventFd;
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

static IN_EPOLL_WAIT: AtomicBool = AtomicBool::new(false);

/// Test-only signal indicating whether some thread is currently blocked in `epoll_wait`.
///
/// This is not a synchronization primitive; it only exists to make it possible
/// to deterministically reproduce and test the "GC request while blocked in
/// epoll_wait" scenario.
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
  fn to_epoll_events(self) -> u32 {
    let mut events = 0;
    if self.contains(Self::READABLE) {
      events |= libc::EPOLLIN as u32;
    }
    if self.contains(Self::WRITABLE) {
      events |= libc::EPOLLOUT as u32;
    }
    // Always report error/hangup conditions to the callback.
    events | (libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLRDHUP) as u32
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
  epoll: Epoll,
  wake: EventFd,
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
  // Safety: `data` is allocated via `Box::into_raw(IoTask)` in `wait`.
  let task = unsafe { Box::from_raw(data as *mut IoTask) };
  if task.active.load(Ordering::Acquire) {
    (task.cb)(task.events, task.data);
  }
}

impl Reactor {
  pub fn new() -> io::Result<Self> {
    let epoll = Epoll::new()?;
    let wake = EventFd::new()?;
    epoll.ctl_add(wake.as_raw_fd(), libc::EPOLLIN as u32, WAKE_TOKEN)?;

    Ok(Self {
      epoll,
      wake,
      next_id: AtomicU64::new(1),
      watchers_count: AtomicUsize::new(0),
      watchers: Mutex::new(HashMap::new()),
    })
  }

  pub fn wake(&self) {
    self.wake.wake();
  }

  pub fn drain_wake(&self) -> io::Result<()> {
    self.wake.drain()
  }

  pub fn has_watchers(&self) -> bool {
    self.watchers_count.load(Ordering::Acquire) > 0
  }

  pub fn register(&self, fd: RawFd, interest: Interest, task: Task) -> io::Result<WatcherId> {
    let id = WatcherId(self.next_id.fetch_add(1, Ordering::Relaxed));
    self.epoll.ctl_add(fd, interest.to_epoll_events(), id.0)?;
    self.watchers.lock().unwrap().insert(
      id,
      Watcher {
        fd,
        interest,
        kind: WatcherKind::Task(task),
      },
    );
    self.watchers_count.fetch_add(1, Ordering::Release);
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
    let id = WatcherId(self.next_id.fetch_add(1, Ordering::Relaxed));
    let interest = rt_interests_to_interest(interests);
    self.epoll.ctl_add(fd, interest.to_epoll_events(), id.0)?;
    self.watchers.lock().unwrap().insert(
      id,
      Watcher {
        fd,
        interest,
        kind: WatcherKind::Io(IoWatcher {
          interests,
          cb,
          data,
          active: Arc::new(AtomicBool::new(true)),
        }),
      },
    );
    self.watchers_count.fetch_add(1, Ordering::Release);
    self.wake();
    Ok(id)
  }

  pub fn update_io(&self, id: WatcherId, interests: u32) -> bool {
    let interest = rt_interests_to_interest(interests);
    let mut watchers = self.watchers.lock().unwrap();
    let Some(watcher) = watchers.get_mut(&id) else {
      return false;
    };

    if self
      .epoll
      .ctl_mod(watcher.fd, interest.to_epoll_events(), id.0)
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
    let watcher = self.watchers.lock().unwrap().remove(&id);
    let Some(watcher) = watcher else {
      return false;
    };
    if let WatcherKind::Io(io) = watcher.kind {
      io.active.store(false, Ordering::Release);
    }
    let _ = self.epoll.ctl_del(watcher.fd);
    self.watchers_count.fetch_sub(1, Ordering::Release);
    self.wake();
    true
  }

  pub fn clear_watchers(&self) {
    let mut watchers = self.watchers.lock().unwrap();
    if watchers.is_empty() {
      return;
    }

    for (_id, watcher) in watchers.drain() {
      let _ = self.epoll.ctl_del(watcher.fd);
    }
    self.watchers_count.store(0, Ordering::Release);
    self.wake();
  }

  pub fn wait(&self, timeout_ms: i32) -> io::Result<Vec<Task>> {
    const MAX_EVENTS: usize = 64;
    let mut events: [libc::epoll_event; MAX_EVENTS] = unsafe { std::mem::zeroed() };

    IN_EPOLL_WAIT.store(true, Ordering::Release);
    let res = self.epoll.wait(&mut events, timeout_ms);
    IN_EPOLL_WAIT.store(false, Ordering::Release);
    let n = res?;

    if n == 0 {
      return Ok(Vec::new());
    }

    let mut needs_wake_drain = false;
    let mut ready_tokens: Vec<(WatcherId, u32)> = Vec::new();
    for ev in events.iter().take(n) {
      let token = ev.u64;
      if token == WAKE_TOKEN {
        needs_wake_drain = true;
      } else {
        ready_tokens.push((WatcherId(token), ev.events));
      }
    }

    if needs_wake_drain {
      self.wake.drain()?;
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
            let ready = epoll_events_to_rt(events);
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
            tasks.push(Task::new(run_io_task, Box::into_raw(task) as *mut u8));
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
