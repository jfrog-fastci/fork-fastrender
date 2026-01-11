use crate::async_rt::reactor::Reactor;
use crate::async_rt::timer::Timers;
use crate::async_rt::Interest;
use crate::async_rt::Task;
use crate::async_rt::TimerId;
use crate::async_rt::WatcherId;
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

pub struct EventLoop {
  poll_lock: Mutex<()>,
  microtasks: Mutex<VecDeque<Task>>,
  macrotasks: Mutex<VecDeque<Task>>,
  timers: Timers,
  reactor: Reactor,
}

struct UnparkOnDrop;

impl Drop for UnparkOnDrop {
  fn drop(&mut self) {
    crate::threading::set_parked(false);
    crate::threading::safepoint_poll();
  }
}

impl EventLoop {
  pub fn new() -> std::io::Result<Self> {
    Ok(Self {
      poll_lock: Mutex::new(()),
      microtasks: Mutex::new(VecDeque::new()),
      macrotasks: Mutex::new(VecDeque::new()),
      timers: Timers::new(),
      reactor: Reactor::new()?,
    })
  }

  pub fn enqueue_microtask(&self, task: Task) {
    self.microtasks.lock().unwrap().push_back(task);
    self.reactor.wake();
  }

  pub fn enqueue_macrotask(&self, task: Task) {
    self.macrotasks.lock().unwrap().push_back(task);
    self.reactor.wake();
  }

  pub fn schedule_timer(&self, deadline: Instant, task: Task) -> TimerId {
    let id = self.timers.schedule(deadline, task);
    // If this timer is sooner than a currently-blocking epoll_wait timeout, we
    // need to wake so `poll` recomputes its timeout.
    self.reactor.wake();
    id
  }

  pub fn cancel_timer(&self, id: TimerId) -> bool {
    let existed = self.timers.cancel(id);
    if existed {
      self.reactor.wake();
    }
    existed
  }

  pub fn register_fd(&self, fd: RawFd, interest: Interest, task: Task) -> std::io::Result<WatcherId> {
    self.reactor.register(fd, interest, task)
  }

  pub fn register_io(
    &self,
    fd: RawFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
  ) -> io::Result<WatcherId> {
    self.reactor.register_io(fd, interests, cb, data)
  }

  pub fn update_io(&self, id: WatcherId, interests: u32) -> bool {
    self.reactor.update_io(id, interests)
  }

  pub fn deregister_fd(&self, id: WatcherId) -> bool {
    self.reactor.deregister(id)
  }

  pub(crate) fn wake(&self) {
    self.reactor.wake();
  }

  fn flush_due_timers(&self) {
    let now = Instant::now();
    let due = self.timers.drain_due(now);
    if due.is_empty() {
      return;
    }
    let mut mq = self.macrotasks.lock().unwrap();
    mq.extend(due);
  }

  fn pop_macrotask(&self) -> Option<Task> {
    self.macrotasks.lock().unwrap().pop_front()
  }

  fn has_microtasks(&self) -> bool {
    !self.microtasks.lock().unwrap().is_empty()
  }

  fn drain_microtasks(&self) {
    loop {
      let tasks: Vec<Task> = {
        let mut q = self.microtasks.lock().unwrap();
        if q.is_empty() {
          break;
        }
        q.drain(..).collect()
      };
      for task in tasks {
        task.run();
      }
    }
  }

  fn has_pending_work(&self) -> bool {
    if self.reactor.has_watchers() {
      return true;
    }
    if self.timers.has_timers() {
      return true;
    }
    if !self.microtasks.lock().unwrap().is_empty() {
      return true;
    }
    if !self.macrotasks.lock().unwrap().is_empty() {
      return true;
    }
    false
  }

  fn compute_timeout_ms(&self) -> i32 {
    let Some(deadline) = self.timers.next_deadline() else {
      // No timers; block indefinitely for I/O or wakeups.
      return -1;
    };

    let now = Instant::now();
    if deadline <= now {
      return 0;
    }

    let dur = deadline.duration_since(now);
    let mut ms = dur.as_millis();
    if ms == 0 && dur > Duration::ZERO {
      ms = 1;
    }
    ms.min(i32::MAX as u128) as i32
  }

  pub fn poll(&self) -> bool {
    let _guard = self.poll_lock.lock().unwrap();

    loop {
      // 1. Promote due timers into the macrotask queue.
      self.flush_due_timers();

      // 2. Execute at most one macrotask (if any).
      if let Some(task) = self.pop_macrotask() {
        task.run();

        // 3. Microtask checkpoint.
        self.drain_microtasks();
        return self.has_pending_work();
      }

      // No macrotasks. If there are microtasks, run them without blocking.
      if self.has_microtasks() {
        self.drain_microtasks();
        return self.has_pending_work();
      }

      // No ready work.
      if !self.reactor.has_watchers() && !self.timers.has_timers() {
        return false;
      }

      // 4. Block until I/O readiness, timer, or wakeup.
      let timeout_ms = self.compute_timeout_ms();
      // Integrate with the stop-the-world safepoint coordinator: while blocked
      // in `epoll_wait`, treat this thread as quiescent so GC doesn't need to
      // wake it.
      let ready = if timeout_ms == 0 {
        self.reactor.wait(timeout_ms).expect("epoll_wait failed")
      } else {
        crate::threading::set_parked(true);
        let _unpark = UnparkOnDrop;
        self.reactor.wait(timeout_ms).expect("epoll_wait failed")
      };
      if !ready.is_empty() {
        self.macrotasks.lock().unwrap().extend(ready);
      }
      // Loop to run the newly-ready tasks (or newly-due timers).
    }
  }
  pub(crate) fn reset_for_tests(&self) {
    let _guard = self.poll_lock.lock().unwrap();

    self.microtasks.lock().unwrap().clear();
    self.macrotasks.lock().unwrap().clear();
    self.timers.clear();
    self.reactor.clear_watchers();
    let _ = self.reactor.drain_wake();
  }
}
