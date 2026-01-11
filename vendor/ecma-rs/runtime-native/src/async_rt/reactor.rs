use crate::async_rt::Task;
use crate::platform::linux_epoll::Epoll;
use crate::platform::linux_epoll::EventFd;
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

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
  task: Task,
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
        task,
      },
    );
    self.watchers_count.fetch_add(1, Ordering::Release);
    self.wake();
    Ok(id)
  }

  pub fn deregister(&self, id: WatcherId) -> bool {
    let watcher = self.watchers.lock().unwrap().remove(&id);
    let Some(watcher) = watcher else {
      return false;
    };
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
    let n = self.epoll.wait(&mut events, timeout_ms)?;

    if n == 0 {
      return Ok(Vec::new());
    }

    let mut needs_wake_drain = false;
    let mut ready_tokens: Vec<WatcherId> = Vec::new();
    for ev in events.iter().take(n) {
      let token = ev.u64;
      if token == WAKE_TOKEN {
        needs_wake_drain = true;
      } else {
        ready_tokens.push(WatcherId(token));
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
    for id in ready_tokens {
      if let Some(watcher) = watchers.get(&id) {
        let _ = watcher.interest; // Placeholder for future event filtering.
        tasks.push(watcher.task.clone());
      }
    }
    Ok(tasks)
  }
}
