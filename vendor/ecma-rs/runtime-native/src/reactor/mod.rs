#![doc = include_str!("../../docs/reactor.md")]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Token associated with a registered I/O source.
///
/// The reactor guarantees that [`Reactor::poll`] returns at most one [`Event`] per [`Token`]
/// per call.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Token(pub usize);

impl Token {
  /// Reserved token used by the reactor's internal [`Waker`].
  pub const WAKE: Token = Token(usize::MAX);
}

/// Readiness interests for a registered I/O source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Interest(u8);

impl Interest {
  pub const READABLE: Interest = Interest(0b01);
  pub const WRITABLE: Interest = Interest(0b10);

  pub const fn empty() -> Interest {
    Interest(0)
  }

  pub const fn is_empty(self) -> bool {
    self.0 == 0
  }

  pub const fn contains(self, other: Interest) -> bool {
    (self.0 & other.0) == other.0
  }

  pub const fn union(self, other: Interest) -> Interest {
    Interest(self.0 | other.0)
  }
}

impl std::ops::BitOr for Interest {
  type Output = Interest;

  fn bitor(self, rhs: Interest) -> Interest {
    self.union(rhs)
  }
}

/// Readiness event returned by the reactor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Event {
  pub token: Token,
  pub readable: bool,
  pub writable: bool,

  /// The read side of the underlying stream was closed (EOF / peer close).
  pub read_closed: bool,

  /// The write side of the underlying stream was closed.
  pub write_closed: bool,

  /// An error was reported by the OS for this source.
  pub error: bool,
}

impl Event {
  fn merge_from(&mut self, other: Event) {
    debug_assert_eq!(self.token, other.token);
    self.readable |= other.readable;
    self.writable |= other.writable;
    self.read_closed |= other.read_closed;
    self.write_closed |= other.write_closed;
    self.error |= other.error;
  }
}

/// Handle that can wake a thread blocked in [`Reactor::poll`].
///
/// This type is `Clone` and can be sent to other threads.
#[derive(Clone)]
pub struct Waker {
  inner: Arc<WakerInner>,
}

#[cfg(target_os = "linux")]
struct WakerInner {
  eventfd: OwnedFd,
}

#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]
struct WakerInner {
  kqueue: OwnedFd,
  ident: libc::uintptr_t,
}

#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
)))]
struct WakerInner;

impl Waker {
  pub fn wake(&self) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
      let val: u64 = 1;
      let rc = unsafe {
        libc::write(
          self.inner.eventfd.as_raw_fd(),
          &val as *const u64 as *const libc::c_void,
          std::mem::size_of::<u64>(),
        )
      };
      if rc == -1 {
        let err = io::Error::last_os_error();
        // Counter overflow is practically impossible; treat EAGAIN as coalescing.
        if err.kind() == io::ErrorKind::WouldBlock {
          return Ok(());
        }
        return Err(err);
      }
      return Ok(());
    }

    #[cfg(any(
      target_os = "macos",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "dragonfly"
    ))]
    {
      let mut kev = libc::kevent {
        ident: self.inner.ident,
        filter: libc::EVFILT_USER,
        flags: 0,
        fflags: libc::NOTE_TRIGGER,
        data: 0,
        // Preserve the udata token (some platforms may treat it as part of the change record).
        udata: (Token::WAKE.0 as usize) as *mut libc::c_void,
      };
      let rc = unsafe {
        libc::kevent(
          self.inner.kqueue.as_raw_fd(),
          &kev as *const libc::kevent,
          1,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc == -1 {
        return Err(io::Error::last_os_error());
      }
      return Ok(());
    }

    #[cfg(not(any(
      target_os = "linux",
      target_os = "macos",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "dragonfly"
    )))]
    {
      let _ = self;
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "waker is only supported on epoll/kqueue platforms",
      ))
    }
  }
}

/// Cross-platform reactor providing edge-triggered readiness notifications.
pub struct Reactor {
  sys: sys::ReactorSys,
  waker: Waker,
}

impl Reactor {
  pub fn new() -> io::Result<Reactor> {
    let (sys, waker) = sys::ReactorSys::new_with_waker()?;
    Ok(Reactor { sys, waker })
  }

  pub fn waker(&self) -> Waker {
    self.waker.clone()
  }

  pub fn register(&mut self, fd: BorrowedFd<'_>, token: Token, interest: Interest) -> io::Result<()> {
    if token == Token::WAKE {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Token::WAKE is reserved for the reactor waker",
      ));
    }
    ensure_nonblocking(fd)?;
    self.sys.register(fd.as_raw_fd(), token, interest)
  }

  pub fn reregister(
    &mut self,
    fd: BorrowedFd<'_>,
    token: Token,
    interest: Interest,
  ) -> io::Result<()> {
    if token == Token::WAKE {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Token::WAKE is reserved for the reactor waker",
      ));
    }
    ensure_nonblocking(fd)?;
    self.sys.reregister(fd.as_raw_fd(), token, interest)
  }

  pub fn deregister(&mut self, fd: BorrowedFd<'_>) -> io::Result<()> {
    self.sys.deregister(fd.as_raw_fd())
  }

  /// Polls for events and appends them to `events` (clearing it first).
  ///
  /// Returns the number of events written to `events`.
  pub fn poll(&mut self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<usize> {
    events.clear();

    // To guarantee monotonic timeouts in the face of EINTR, we compute an absolute deadline and
    // retry as needed with a recomputed relative timeout.
    let deadline = timeout.map(|d| Instant::now() + d);

    let mut scratch = self.sys.poll_raw(deadline)?;

    // Drain wake events before returning to keep edge-triggered semantics (eventfd counter/kevent
    // user event).
    if let Some(wake_idx) = scratch.iter().position(|e| e.token == Token::WAKE) {
      // Remove wake event from scratch (we'll add it back exactly once at the end).
      scratch.swap_remove(wake_idx);
      self.sys.drain_waker()?;
      events.push(Event {
        token: Token::WAKE,
        readable: true,
        writable: false,
        read_closed: false,
        write_closed: false,
        error: false,
      });
    }

    for ev in scratch {
      if let Some(existing) = events.iter_mut().find(|e| e.token == ev.token) {
        existing.merge_from(ev);
      } else {
        events.push(ev);
      }
    }

    Ok(events.len())
  }
}

fn ensure_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
  let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
  if flags == -1 {
    return Err(io::Error::last_os_error());
  }
  if flags & libc::O_NONBLOCK == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "reactor requires all registered file descriptors to be O_NONBLOCK (edge-triggered contract)",
    ));
  }
  Ok(())
}

#[cfg(target_os = "linux")]
mod sys {
  use super::{Event, Interest, Token};
  use std::io;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
  use std::time::Instant;

  pub(super) struct ReactorSys {
    epoll: OwnedFd,
    eventfd: OwnedFd,
  }

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
      if epoll_fd == -1 {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: just created fd.
      let epoll = unsafe { OwnedFd::from_raw_fd(epoll_fd) };

      let eventfd_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
      if eventfd_fd == -1 {
        return Err(io::Error::last_os_error());
      }
      let eventfd = unsafe { OwnedFd::from_raw_fd(eventfd_fd) };

      let mut sys = ReactorSys {
        epoll,
        eventfd,
      };

      // Register the eventfd for wakeups.
      sys.register_raw(sys.eventfd.as_raw_fd(), Token::WAKE, Interest::READABLE)?;

      let waker = super::Waker {
        inner: std::sync::Arc::new(super::WakerInner {
          eventfd: sys.eventfd.try_clone()?,
        }),
      };

      Ok((sys, waker))
    }

    pub(super) fn register(&mut self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      self.ctl(libc::EPOLL_CTL_ADD, fd, token, interest)
    }

    pub(super) fn reregister(
      &mut self,
      fd: RawFd,
      token: Token,
      interest: Interest,
    ) -> io::Result<()> {
      self.ctl(libc::EPOLL_CTL_MOD, fd, token, interest)
    }

    pub(super) fn deregister(&mut self, fd: RawFd) -> io::Result<()> {
      let rc = unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
      if rc == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    }

    fn register_raw(&mut self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      // Used for internal fds; doesn't enforce nonblocking at this layer.
      self.ctl(libc::EPOLL_CTL_ADD, fd, token, interest)
    }

    fn ctl(&mut self, op: i32, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      let mut ev = libc::epoll_event {
        events: interest_to_epoll(interest),
        u64: token.0 as u64,
      };
      let rc = unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut ev as *mut libc::epoll_event) };
      if rc == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    }

    pub(super) fn poll_raw(&mut self, deadline: Option<Instant>) -> io::Result<Vec<Event>> {
      let mut out = Vec::with_capacity(64);
      let mut buf = [libc::epoll_event { events: 0, u64: 0 }; 64];

      loop {
        let timeout_ms = match deadline {
          None => -1,
          Some(d) => {
            let now = Instant::now();
            if now >= d {
              0
            } else {
              let remaining = d - now;
              // Round up to avoid spuriously timing out before the deadline.
              let mut ms = remaining.as_millis();
              if ms == 0 {
                ms = 1;
              }
              (ms.min(i32::MAX as u128)) as i32
            }
          }
        };

        let n = unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), buf.as_mut_ptr(), buf.len() as i32, timeout_ms) };
        if n == -1 {
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(err);
        }

        for kev in &buf[..(n as usize)] {
          let token = Token(kev.u64 as usize);
          out.push(epoll_to_event(token, kev.events));
        }

        return Ok(out);
      }
    }

    pub(super) fn drain_waker(&mut self) -> io::Result<()> {
      let mut buf: u64 = 0;
      loop {
        let rc = unsafe {
          libc::read(
            self.eventfd.as_raw_fd(),
            &mut buf as *mut u64 as *mut libc::c_void,
            std::mem::size_of::<u64>(),
          )
        };
        if rc == -1 {
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
          }
          return Err(err);
        }
      }
    }
  }

  fn interest_to_epoll(interest: Interest) -> u32 {
    let mut out = libc::EPOLLET as u32;
    if interest.contains(Interest::READABLE) {
      out |= libc::EPOLLIN as u32;
      out |= libc::EPOLLRDHUP as u32;
    }
    if interest.contains(Interest::WRITABLE) {
      out |= libc::EPOLLOUT as u32;
    }
    out
  }

  fn epoll_to_event(token: Token, events: u32) -> Event {
    let read_closed = (events & (libc::EPOLLRDHUP as u32 | libc::EPOLLHUP as u32)) != 0;
    let write_closed = (events & (libc::EPOLLHUP as u32)) != 0;
    let error = (events & (libc::EPOLLERR as u32)) != 0;

    Event {
      token,
      readable: (events & (libc::EPOLLIN as u32)) != 0 || read_closed,
      writable: (events & (libc::EPOLLOUT as u32)) != 0,
      read_closed,
      write_closed,
      error,
    }
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
  use super::{Event, Interest, Token};
  use std::io;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
  use std::time::Instant;

  pub(super) struct ReactorSys {
    kqueue: OwnedFd,
  }

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      let kq = unsafe { libc::kqueue() };
      if kq == -1 {
        return Err(io::Error::last_os_error());
      }
      let kqueue = unsafe { OwnedFd::from_raw_fd(kq) };

      let ident: libc::uintptr_t = 1;
      let mut kev = libc::kevent {
        ident,
        filter: libc::EVFILT_USER,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: libc::NOTE_FFNOP,
        data: 0,
        udata: (Token::WAKE.0 as usize) as *mut libc::c_void,
      };
      let rc = unsafe {
        libc::kevent(
          kqueue.as_raw_fd(),
          &kev as *const libc::kevent,
          1,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc == -1 {
        return Err(io::Error::last_os_error());
      }

      let sys = ReactorSys {
        kqueue: kqueue.try_clone()?,
      };

      let waker = super::Waker {
        inner: std::sync::Arc::new(super::WakerInner {
          kqueue,
          ident,
        }),
      };

      Ok((sys, waker))
    }

    pub(super) fn register(&mut self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      self.update(fd, token, interest, false)
    }

    pub(super) fn reregister(
      &mut self,
      fd: RawFd,
      token: Token,
      interest: Interest,
    ) -> io::Result<()> {
      self.update(fd, token, interest, true)
    }

    pub(super) fn deregister(&mut self, fd: RawFd) -> io::Result<()> {
      // Best-effort delete; ignore ENOENT.
      for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
        let mut kev = libc::kevent {
          ident: fd as libc::uintptr_t,
          filter,
          flags: libc::EV_DELETE,
          fflags: 0,
          data: 0,
          udata: std::ptr::null_mut(),
        };
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
        if rc == -1 {
          let err = io::Error::last_os_error();
          if err.raw_os_error() == Some(libc::ENOENT) {
            continue;
          }
          return Err(err);
        }
      }
      Ok(())
    }

    fn update(&mut self, fd: RawFd, token: Token, interest: Interest, clear_existing: bool) -> io::Result<()> {
      if clear_existing {
        self.deregister(fd)?;
      }

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

      if changes.is_empty() {
        return Ok(());
      }

      let rc = unsafe {
        libc::kevent(
          self.kqueue.as_raw_fd(),
          changes.as_ptr(),
          changes.len() as i32,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    }

    pub(super) fn poll_raw(&mut self, deadline: Option<Instant>) -> io::Result<Vec<Event>> {
      let mut out = Vec::with_capacity(64);
      let mut buf = [libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
      }; 64];

      loop {
        let mut ts_storage = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let ts_ptr = match deadline {
          None => std::ptr::null(),
          Some(d) => {
            let now = Instant::now();
            if now >= d {
              ts_storage.tv_sec = 0;
              ts_storage.tv_nsec = 0;
            } else {
              let remaining = d - now;
              ts_storage.tv_sec = remaining.as_secs() as libc::time_t;
              ts_storage.tv_nsec = remaining.subsec_nanos() as libc::c_long;
            }
            &ts_storage as *const libc::timespec
          }
        };

        let n = unsafe {
          libc::kevent(
            self.kqueue.as_raw_fd(),
            std::ptr::null(),
            0,
            buf.as_mut_ptr(),
            buf.len() as i32,
            ts_ptr,
          )
        };
        if n == -1 {
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(err);
        }

        for kev in &buf[..(n as usize)] {
          let token = Token(kev.udata as usize);
          out.push(kevent_to_event(token, kev));
        }

        return Ok(out);
      }
    }

    pub(super) fn drain_waker(&mut self) -> io::Result<()> {
      // EVFILT_USER events are one-shot and are cleared by being returned.
      Ok(())
    }
  }

  fn make_kevent(fd: RawFd, filter: i16, token: Token, flags: u16) -> libc::kevent {
    libc::kevent {
      ident: fd as libc::uintptr_t,
      filter,
      flags,
      fflags: 0,
      data: 0,
      udata: (token.0 as usize) as *mut libc::c_void,
    }
  }

  fn kevent_to_event(token: Token, kev: &libc::kevent) -> Event {
    let error = (kev.flags & libc::EV_ERROR) != 0;
    let read_closed = (kev.flags & libc::EV_EOF) != 0 && kev.filter == libc::EVFILT_READ;
    let write_closed = (kev.flags & libc::EV_EOF) != 0 && kev.filter == libc::EVFILT_WRITE;

    let mut ev = Event {
      token,
      readable: kev.filter == libc::EVFILT_READ,
      writable: kev.filter == libc::EVFILT_WRITE,
      read_closed,
      write_closed,
      error,
    };

    // When EOF is reported, surface it as a readable/writable event for the appropriate direction.
    if read_closed {
      ev.readable = true;
    }
    if write_closed {
      ev.writable = true;
    }

    ev
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
  use super::{Event, Interest, Token};
  use std::io;
  use std::os::fd::RawFd;
  use std::time::Instant;

  pub(super) struct ReactorSys;

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "runtime-native reactor is only supported on epoll/kqueue platforms",
      ))
    }

    pub(super) fn register(&mut self, _fd: RawFd, _token: Token, _interest: Interest) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn reregister(&mut self, _fd: RawFd, _token: Token, _interest: Interest) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn deregister(&mut self, _fd: RawFd) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn poll_raw(&mut self, _deadline: Option<Instant>) -> io::Result<Vec<Event>> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn drain_waker(&mut self) -> io::Result<()> {
      Ok(())
    }
  }
}
