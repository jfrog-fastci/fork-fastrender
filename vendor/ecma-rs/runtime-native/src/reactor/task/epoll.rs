use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::Duration;

use super::{Interest, PollOutcome};

const MAX_EPOLL_EVENTS: usize = 1024;
const WAKEUP_GENERATION: u32 = 0;

pub struct Reactor {
  inner: Arc<Inner>,
}

impl Clone for Reactor {
  fn clone(&self) -> Self {
    Self {
      inner: Arc::clone(&self.inner),
    }
  }
}

struct Inner {
  epoll_fd: OwnedFd,
  wakeup_fd: OwnedFd,

  next_generation: AtomicU32,
  state: Mutex<State>,
}

#[derive(Default)]
struct State {
  entries: HashMap<RawFd, Entry>,
}

struct Entry {
  generation: u32,
  interest: Interest,
  readable_waker: Option<Waker>,
  writable_waker: Option<Waker>,
}

impl Reactor {
  pub fn new() -> io::Result<Self> {
    // SAFETY: syscall, checked for -1.
    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll_fd < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `epoll_fd` is freshly created and owned by us.
    let epoll_fd = unsafe { OwnedFd::from_raw_fd(epoll_fd) };

    // SAFETY: syscall, checked for -1.
    let wakeup_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if wakeup_fd < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `wakeup_fd` is freshly created and owned by us.
    let wakeup_fd = unsafe { OwnedFd::from_raw_fd(wakeup_fd) };

    // Register the wakeup fd with epoll so other threads can interrupt `epoll_wait`.
    epoll_ctl_add(
      epoll_fd.as_raw_fd(),
      wakeup_fd.as_raw_fd(),
      libc::EPOLLIN as u32,
      make_token(wakeup_fd.as_raw_fd(), WAKEUP_GENERATION),
    )?;

    Ok(Self {
      inner: Arc::new(Inner {
        epoll_fd,
        wakeup_fd,
        next_generation: AtomicU32::new(WAKEUP_GENERATION + 1),
        state: Mutex::new(State::default()),
      }),
    })
  }

  pub fn register(&self, fd: RawFd, interest: Interest, waker: &Waker) -> io::Result<()> {
    let mut state = self.inner.state.lock().unwrap();
    self.register_locked(&mut state, fd, interest, waker)
  }

  fn register_locked(
    &self,
    state: &mut State,
    fd: RawFd,
    interest: Interest,
    waker: &Waker,
  ) -> io::Result<()> {
    if interest.is_empty() {
      return Ok(());
    }

    match state.entries.entry(fd) {
      std::collections::hash_map::Entry::Vacant(vacant) => {
        let readable_waker = interest.contains(Interest::READABLE).then(|| waker.clone());
        let writable_waker = interest.contains(Interest::WRITABLE).then(|| waker.clone());
        let desired_interest = desired_interest_from_wakers(&readable_waker, &writable_waker);
        debug_assert!(!desired_interest.is_empty());

        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        epoll_ctl_add(
          self.inner.epoll_fd.as_raw_fd(),
          fd,
          interest_to_epoll_events(desired_interest),
          make_token(fd, generation),
        )?;

        vacant.insert(Entry {
          generation,
          interest: desired_interest,
          readable_waker,
          writable_waker,
        });
        Ok(())
      }
      std::collections::hash_map::Entry::Occupied(mut occupied) => {
        let entry = occupied.get_mut();
        let mut new_readable_waker = entry.readable_waker.clone();
        let mut new_writable_waker = entry.writable_waker.clone();
        if interest.contains(Interest::READABLE) {
          new_readable_waker = Some(waker.clone());
        }
        if interest.contains(Interest::WRITABLE) {
          new_writable_waker = Some(waker.clone());
        }

        let desired_interest =
          desired_interest_from_wakers(&new_readable_waker, &new_writable_waker);
        debug_assert!(!desired_interest.is_empty());

        if desired_interest == entry.interest {
          entry.readable_waker = new_readable_waker;
          entry.writable_waker = new_writable_waker;
          return Ok(());
        }

        match epoll_ctl_mod(
          self.inner.epoll_fd.as_raw_fd(),
          fd,
          interest_to_epoll_events(desired_interest),
          make_token(fd, entry.generation),
        ) {
          Ok(()) => {}
          Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {
            let new_generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
            epoll_ctl_add(
              self.inner.epoll_fd.as_raw_fd(),
              fd,
              interest_to_epoll_events(desired_interest),
              make_token(fd, new_generation),
            )?;
            entry.generation = new_generation;
          }
          Err(err) => return Err(err),
        }

        entry.interest = desired_interest;
        entry.readable_waker = new_readable_waker;
        entry.writable_waker = new_writable_waker;
        Ok(())
      }
    }
  }

  pub fn deregister(&self, fd: RawFd, interest: Interest) -> io::Result<()> {
    let mut state = self.inner.state.lock().unwrap();
    self.deregister_locked(&mut state, fd, interest)
  }

  fn deregister_locked(&self, state: &mut State, fd: RawFd, interest: Interest) -> io::Result<()> {
    match state.entries.entry(fd) {
      std::collections::hash_map::Entry::Vacant(_) => Ok(()),
      std::collections::hash_map::Entry::Occupied(mut occupied) => {
        let entry = occupied.get_mut();

        let mut new_readable_waker = entry.readable_waker.clone();
        let mut new_writable_waker = entry.writable_waker.clone();
        if interest.contains(Interest::READABLE) {
          new_readable_waker = None;
        }
        if interest.contains(Interest::WRITABLE) {
          new_writable_waker = None;
        }

        let desired_interest =
          desired_interest_from_wakers(&new_readable_waker, &new_writable_waker);
        if desired_interest.is_empty() {
          // Remove from the epoll set.
          match epoll_ctl_del(self.inner.epoll_fd.as_raw_fd(), fd) {
            Ok(()) => {}
            Err(err) if matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EBADF)) => {}
            Err(err) => return Err(err),
          }
          occupied.remove();
          return Ok(());
        }

        if entry.interest == desired_interest {
          entry.readable_waker = new_readable_waker;
          entry.writable_waker = new_writable_waker;
          return Ok(());
        }

        match epoll_ctl_mod(
          self.inner.epoll_fd.as_raw_fd(),
          fd,
          interest_to_epoll_events(desired_interest),
          make_token(fd, entry.generation),
        ) {
          Ok(()) => {}
          Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {
            let new_generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
            epoll_ctl_add(
              self.inner.epoll_fd.as_raw_fd(),
              fd,
              interest_to_epoll_events(desired_interest),
              make_token(fd, new_generation),
            )?;
            entry.generation = new_generation;
          }
          Err(err) => return Err(err),
        }

        entry.interest = desired_interest;
        entry.readable_waker = new_readable_waker;
        entry.writable_waker = new_writable_waker;
        Ok(())
      }
    }
  }

  pub fn notify(&self) -> io::Result<()> {
    let val: u64 = 1;
    let buf = val.to_ne_bytes();

    loop {
      // SAFETY: `wakeup_fd` is owned by this reactor and the pointer is valid for 8 bytes.
      let res = unsafe {
        libc::write(
          self.inner.wakeup_fd.as_raw_fd(),
          buf.as_ptr().cast::<libc::c_void>(),
          buf.len(),
        )
      };

      if res >= 0 {
        return Ok(());
      }

      let err = io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EINTR) => continue,
        // Ignore if the eventfd counter is saturated; any wake is enough.
        Some(libc::EAGAIN) => return Ok(()),
        _ => return Err(err),
      }
    }
  }

  pub fn poll(&self, timeout: Option<Duration>) -> io::Result<PollOutcome> {
    let timeout_ms: libc::c_int = match timeout {
      None => -1,
      Some(dur) => duration_to_epoll_timeout_ms(dur),
    };

    let mut events = vec![unsafe { std::mem::zeroed::<libc::epoll_event>() }; MAX_EPOLL_EVENTS];

    let n_events = loop {
      // SAFETY: `events` is a valid, writable buffer for `events.len()` epoll_event entries.
      let res = unsafe {
        libc::epoll_wait(
          self.inner.epoll_fd.as_raw_fd(),
          events.as_mut_ptr(),
          events.len().try_into().unwrap(),
          timeout_ms,
        )
      };

      if res >= 0 {
        break res as usize;
      }

      let err = io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      return Err(err);
    };

    let mut outcome = PollOutcome::default();
    let mut wakers_to_wake: Vec<Waker> = Vec::new();

    for event in &events[..n_events] {
      let token = event.u64;
      let (fd, generation) = split_token(token);

      if generation == WAKEUP_GENERATION && fd == self.inner.wakeup_fd.as_raw_fd() {
        outcome.was_woken_by_notify = true;
        drain_eventfd(self.inner.wakeup_fd.as_raw_fd())?;
        continue;
      }

      outcome.io_events += 1;

      // Capture any wakers to wake while holding the lock, then wake after dropping it.
      let (read_waker, write_waker) = {
        let state = self.inner.state.lock().unwrap();
        let Some(entry) = state.entries.get(&fd) else {
          continue;
        };
        if entry.generation != generation {
          continue;
        }

        let flags = event.events as u32;
        let errorish = flags
          & ((libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLRDHUP) as u32)
          != 0;
        let mut readable = flags & (libc::EPOLLIN as u32) != 0;
        let mut writable = flags & (libc::EPOLLOUT as u32) != 0;
        if errorish {
          readable = true;
          writable = true;
        }

        (
          readable.then(|| entry.readable_waker.clone()).flatten(),
          writable.then(|| entry.writable_waker.clone()).flatten(),
        )
      };

      if let Some(w) = read_waker {
        push_dedup_waker(&mut wakers_to_wake, w);
      }
      if let Some(w) = write_waker {
        push_dedup_waker(&mut wakers_to_wake, w);
      }
    }

    outcome.wakers_woken = wakers_to_wake.len();
    for w in wakers_to_wake {
      w.wake();
    }

    Ok(outcome)
  }
}

fn duration_to_epoll_timeout_ms(dur: Duration) -> libc::c_int {
  if dur.is_zero() {
    return 0;
  }
  let ms = dur.as_millis();
  if ms > libc::c_int::MAX as u128 {
    libc::c_int::MAX
  } else {
    ms as libc::c_int
  }
}

fn make_token(fd: RawFd, generation: u32) -> u64 {
  ((generation as u64) << 32) | (fd as u32 as u64)
}

fn split_token(token: u64) -> (RawFd, u32) {
  let fd = (token & 0xFFFF_FFFF) as u32;
  let generation = (token >> 32) as u32;
  (fd as RawFd, generation)
}

fn desired_interest_from_wakers(readable: &Option<Waker>, writable: &Option<Waker>) -> Interest {
  let mut interest = Interest::empty();
  if readable.is_some() {
    interest |= Interest::READABLE;
  }
  if writable.is_some() {
    interest |= Interest::WRITABLE;
  }
  interest
}

fn interest_to_epoll_events(interest: Interest) -> u32 {
  let mut events = 0u32;
  if interest.contains(Interest::READABLE) {
    events |= libc::EPOLLIN as u32;
  }
  if interest.contains(Interest::WRITABLE) {
    events |= libc::EPOLLOUT as u32;
  }

  // EPOLLERR/EPOLLHUP are always reported; include RDHUP so sockets get half-close notifications.
  events | libc::EPOLLRDHUP as u32
}

fn push_dedup_waker(out: &mut Vec<Waker>, waker: Waker) {
  if out.iter().any(|w| w.will_wake(&waker)) {
    return;
  }
  out.push(waker);
}

fn drain_eventfd(fd: RawFd) -> io::Result<()> {
  let mut buf = [0u8; 8];
  loop {
    // SAFETY: `buf` is a valid writable buffer and `fd` is expected to be an eventfd.
    let res =
      unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };

    if res >= 0 {
      continue;
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
      Some(libc::EINTR) => continue,
      Some(libc::EAGAIN) => return Ok(()),
      _ => return Err(err),
    }
  }
}

fn epoll_ctl_add(epoll_fd: RawFd, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
  let mut event = libc::epoll_event { events, u64: token };

  // SAFETY: syscall, `event` is valid for `ADD`.
  let res = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event) };
  if res < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn epoll_ctl_mod(epoll_fd: RawFd, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
  let mut event = libc::epoll_event { events, u64: token };

  // SAFETY: syscall, `event` is valid for `MOD`.
  let res = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut event) };
  if res < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn epoll_ctl_del(epoll_fd: RawFd, fd: RawFd) -> io::Result<()> {
  // SAFETY: syscall; `DEL` ignores the event pointer.
  let res = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
  if res < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
