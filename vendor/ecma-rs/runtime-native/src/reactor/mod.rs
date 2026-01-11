#![doc = include_str!("../../docs/reactor.md")]

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

pub mod task;

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
enum WakerInner {
  User { kqueue: OwnedFd, ident: libc::uintptr_t },
  Pipe {
    // Keep a duplicate read-end alive so wake writes never observe a reader-less pipe (EPIPE /
    // SIGPIPE) even if the reactor is dropped before the waker.
    _read_keepalive: OwnedFd,
    write: OwnedFd,
  },
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
      loop {
        let rc = unsafe {
          libc::write(
            self.inner.eventfd.as_raw_fd(),
            &val as *const u64 as *const libc::c_void,
            std::mem::size_of::<u64>(),
          )
        };
        if rc == -1 {
          let err = io::Error::last_os_error();
          match err.kind() {
            io::ErrorKind::Interrupted => continue,
            // Counter overflow is practically impossible; treat EAGAIN as coalescing.
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(err),
          }
        }
        return Ok(());
      }
    }

    #[cfg(any(
      target_os = "macos",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "dragonfly"
    ))]
    {
      match &*self.inner {
        WakerInner::User { kqueue, ident } => loop {
          let kev = libc::kevent {
            ident: *ident,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            // Preserve the udata token (some platforms may treat it as part of the change record).
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
          if rc != -1 {
            return Ok(());
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(err);
        },
        WakerInner::Pipe { write, .. } => loop {
          let buf = [0_u8; 1];
          let rc = unsafe { libc::write(write.as_raw_fd(), buf.as_ptr() as *const libc::c_void, 1) };
          if rc == 1 {
            return Ok(());
          }
          if rc == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
              continue;
            }
            if err.kind() == io::ErrorKind::WouldBlock {
              return Ok(());
            }
            return Err(err);
          }
          return Err(io::Error::new(io::ErrorKind::Other, "pipe wake write returned unexpected value"));
        },
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

  pub fn register(&self, fd: BorrowedFd<'_>, token: Token, interest: Interest) -> io::Result<()> {
    if token == Token::WAKE {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Token::WAKE is reserved for the reactor waker",
      ));
    }
    if interest.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reactor interest must include READABLE and/or WRITABLE",
      ));
    }
    ensure_nonblocking(fd)?;
    self.sys.register(fd.as_raw_fd(), token, interest)
  }

  pub fn reregister(&self, fd: BorrowedFd<'_>, token: Token, interest: Interest) -> io::Result<()> {
    if token == Token::WAKE {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Token::WAKE is reserved for the reactor waker",
      ));
    }
    if interest.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reactor interest must include READABLE and/or WRITABLE",
      ));
    }
    ensure_nonblocking(fd)?;
    self.sys.reregister(fd.as_raw_fd(), token, interest)
  }

  pub fn deregister(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
    self.sys.deregister(fd.as_raw_fd())
  }

  /// Polls for events and appends them to `events` (clearing it first).
  ///
  /// Returns the number of events written to `events`.
  pub fn poll(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<usize> {
    events.clear();

    let mut scratch = self.sys.poll_raw(timeout)?;

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
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
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

  #[cfg(target_os = "linux")]
  mod sys {
  use super::{Event, Interest, Token};
  use std::io;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
  use std::time::{Duration, Instant};

  pub(super) struct ReactorSys {
    epoll: OwnedFd,
    eventfd: OwnedFd,
  }

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      let epoll_fd = loop {
        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd != -1 {
          break epoll_fd;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      };
      // SAFETY: just created fd.
      let epoll = unsafe { OwnedFd::from_raw_fd(epoll_fd) };

      let eventfd_fd = loop {
        let eventfd_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if eventfd_fd != -1 {
          break eventfd_fd;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      };
      let eventfd = unsafe { OwnedFd::from_raw_fd(eventfd_fd) };

      let sys = ReactorSys { epoll, eventfd };

      // Register the eventfd for wakeups.
      sys.register_raw(sys.eventfd.as_raw_fd(), Token::WAKE, Interest::READABLE)?;

      let waker = super::Waker {
        inner: std::sync::Arc::new(super::WakerInner {
          eventfd: sys.eventfd.try_clone()?,
        }),
      };

      Ok((sys, waker))
    }

    pub(super) fn register(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      self.ctl(libc::EPOLL_CTL_ADD, fd, token, interest)
    }

    pub(super) fn reregister(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      self.ctl(libc::EPOLL_CTL_MOD, fd, token, interest)
    }

    pub(super) fn deregister(&self, fd: RawFd) -> io::Result<()> {
      loop {
        let rc = unsafe {
          libc::epoll_ctl(
            self.epoll.as_raw_fd(),
            libc::EPOLL_CTL_DEL,
            fd,
            std::ptr::null_mut(),
          )
        };
        if rc == 0 {
          return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      }
    }

    fn register_raw(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      // Used for internal fds; doesn't enforce nonblocking at this layer.
      self.ctl(libc::EPOLL_CTL_ADD, fd, token, interest)
    }

    fn ctl(&self, op: i32, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      let mut ev = libc::epoll_event {
        events: interest_to_epoll(interest),
        u64: token.0 as u64,
      };
      loop {
        let rc =
          unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut ev as *mut libc::epoll_event) };
        if rc == 0 {
          return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      }
    }

    pub(super) fn poll_raw(&self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
      let mut out = Vec::with_capacity(64);
      let mut buf = [libc::epoll_event { events: 0, u64: 0 }; 64];

      let start = timeout.map(|_| Instant::now());

      loop {
        let timeout_ms = match (timeout, start) {
          (None, _) => -1,
          (Some(total), Some(start)) => {
            let remaining = total.saturating_sub(start.elapsed());
            if remaining.is_zero() {
              0
            } else {
              // Round up to avoid spuriously timing out before the requested duration.
              let ms = (remaining.as_nanos() + 999_999) / 1_000_000;
              (ms.min(i32::MAX as u128)) as i32
            }
          }
          // `start` is always `Some` when `timeout` is `Some`.
          (Some(_), None) => unreachable!(),
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

        if n != 0 {
          return Ok(out);
        }

        // Timed out. If this was a clamped per-wait chunk, keep waiting until the full timeout
        // has elapsed.
        match (timeout, start) {
          (None, _) => unreachable!("epoll_wait returned 0 with infinite timeout"),
          (Some(total), Some(start)) => {
            if start.elapsed() >= total {
              return Ok(out);
            }
          }
          (Some(_), None) => unreachable!(),
        }
      }
    }

    pub(super) fn drain_waker(&self) -> io::Result<()> {
      let mut buf: u64 = 0;
      loop {
        let rc = unsafe {
          libc::read(
            self.eventfd.as_raw_fd(),
            &mut buf as *mut u64 as *mut libc::c_void,
            std::mem::size_of::<u64>(),
          )
        };
        if rc == 8 {
          continue;
        }
        if rc == -1 {
          let err = io::Error::last_os_error();
          match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(err),
          }
        }
        // eventfd reads are expected to be atomic (8 bytes). Treat EOF/short reads as drained.
        return Ok(());
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
      writable: (events & (libc::EPOLLOUT as u32)) != 0 || write_closed,
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
  use crate::sync::GcAwareMutex;
  use std::collections::HashMap;
  use std::io;
  use std::mem::MaybeUninit;
  use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
  use std::time::{Duration, Instant};

  pub(super) struct ReactorSys {
    kqueue: OwnedFd,
    wake_read: Option<OwnedFd>,
    registrations: GcAwareMutex<HashMap<RawFd, Registration>>,
  }

  #[derive(Copy, Clone, Debug, Eq, PartialEq)]
  struct FdIdentity {
    dev: libc::dev_t,
    ino: libc::ino_t,
    file_type: libc::mode_t,
    access_mode: libc::c_int,
  }

  #[derive(Copy, Clone, Debug)]
  struct Registration {
    token: Token,
    interest: Interest,
    identity: FdIdentity,
  }

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      let kq = loop {
        let kq = unsafe { libc::kqueue() };
        if kq != -1 {
          break kq;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      };
      let kqueue = unsafe { OwnedFd::from_raw_fd(kq) };
      set_cloexec(kqueue.as_raw_fd())?;

      let (wake_read, waker_inner) = if cfg!(feature = "force_pipe_wake") {
        let (read, write) = create_pipe()?;
        let _read_keepalive = read.try_clone()?;
        register_pipe_waker(kqueue.as_raw_fd(), read.as_raw_fd())?;
        (
          Some(read),
          super::WakerInner::Pipe {
            _read_keepalive,
            write,
          },
        )
      } else {
        let ident: libc::uintptr_t = 1;
        match register_user_waker(kqueue.as_raw_fd(), ident) {
          Ok(()) => (
            None,
            super::WakerInner::User {
              kqueue: kqueue.try_clone()?,
              ident,
            },
          ),
          Err(err) if is_evfilt_user_unsupported(&err) => {
            let (read, write) = create_pipe()?;
            let _read_keepalive = read.try_clone()?;
            register_pipe_waker(kqueue.as_raw_fd(), read.as_raw_fd())?;
            (
              Some(read),
              super::WakerInner::Pipe {
                _read_keepalive,
                write,
              },
            )
          }
          Err(err) => return Err(err),
        }
      };

      let sys = ReactorSys {
        kqueue,
        wake_read,
        registrations: GcAwareMutex::new(HashMap::new()),
      };

      let waker = super::Waker {
        inner: std::sync::Arc::new(waker_inner),
      };

      Ok((sys, waker))
    }

    pub(super) fn register(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      let mut regs = self.registrations.lock();
      if self.registration_if_fresh_locked(&mut regs, fd)?.is_some() {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }

      let identity = fd_identity(fd)?;
      let changes = changes_for_register(fd, token, interest);
      apply_changes(self.kqueue.as_raw_fd(), &changes)?;

      regs.insert(
        fd,
        Registration {
          token,
          interest,
          identity,
        },
      );
      Ok(())
    }

    pub(super) fn reregister(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
      let mut regs = self.registrations.lock();
      let Some(old) = self.registration_if_fresh_locked(&mut regs, fd)? else {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      };

      let changes = changes_for_reregister(fd, old, token, interest);
      apply_changes(self.kqueue.as_raw_fd(), &changes)?;

      regs.insert(
        fd,
        Registration {
          token,
          interest,
          identity: old.identity,
        },
      );
      Ok(())
    }

    pub(super) fn deregister(&self, fd: RawFd) -> io::Result<()> {
      let mut regs = self.registrations.lock();
      let Some(old) = self.registration_if_fresh_locked(&mut regs, fd)? else {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      };

      let changes = changes_for_deregister(fd, old);
      apply_changes(self.kqueue.as_raw_fd(), &changes)?;

      regs.remove(&fd);
      Ok(())
    }

    fn registration_if_fresh_locked(
      &self,
      regs: &mut HashMap<RawFd, Registration>,
      fd: RawFd,
    ) -> io::Result<Option<Registration>> {
      let Some(reg) = regs.get(&fd).copied() else {
        return Ok(None);
      };

      match fd_identity(fd) {
        Ok(current) if current == reg.identity => Ok(Some(reg)),
        Ok(_) => {
          regs.remove(&fd);
          Ok(None)
        }
        Err(err) if err.raw_os_error() == Some(libc::EBADF) => {
          regs.remove(&fd);
          Ok(None)
        }
        Err(err) => Err(err),
      }
    }

    pub(super) fn poll_raw(&self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
      let mut out = Vec::with_capacity(64);
      let mut buf = [libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
      }; 64];

      let start = timeout.map(|_| Instant::now());

      loop {
        let mut ts_storage = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let ts_ptr = match (timeout, start) {
          (None, _) => std::ptr::null(),
          (Some(total), Some(start)) => {
            let remaining = total.saturating_sub(start.elapsed());
            if remaining.is_zero() {
              ts_storage.tv_sec = 0;
              ts_storage.tv_nsec = 0;
            } else {
              // `timespec.tv_sec` is `time_t` (signed) and may be smaller than `u64`.
              let max_secs = libc::time_t::MAX as u64;
              let secs = remaining.as_secs();
              if secs >= max_secs {
                ts_storage.tv_sec = libc::time_t::MAX;
                ts_storage.tv_nsec = 0;
              } else {
                ts_storage.tv_sec = secs as libc::time_t;
                ts_storage.tv_nsec = remaining.subsec_nanos() as libc::c_long;
              }
            }
            &ts_storage as *const libc::timespec
          }
          // `start` is always `Some` when `timeout` is `Some`.
          (Some(_), None) => unreachable!(),
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

        if n != 0 {
          return Ok(out);
        }

        // Timed out. If this was a clamped per-wait chunk, keep waiting until the full timeout
        // has elapsed.
        match (timeout, start) {
          (None, _) => unreachable!("kevent returned 0 with infinite timeout"),
          (Some(total), Some(start)) => {
            if start.elapsed() >= total {
              return Ok(out);
            }
          }
          (Some(_), None) => unreachable!(),
        }
      }
    }

    pub(super) fn drain_waker(&self) -> io::Result<()> {
      if let Some(read) = &self.wake_read {
        let mut buf = [0u8; 256];
        loop {
          let rc = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
          if rc > 0 {
            continue;
          }
          if rc == 0 {
            break;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          if err.kind() == io::ErrorKind::WouldBlock {
            break;
          }
          return Err(err);
        }
      }
      Ok(())
    }
  }

  fn fd_identity(fd: RawFd) -> io::Result<FdIdentity> {
    let st = loop {
      let mut st = MaybeUninit::<libc::stat>::uninit();
      let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
      if rc != -1 {
        break unsafe { st.assume_init() };
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    };

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

    Ok(FdIdentity {
      dev: st.st_dev,
      ino: st.st_ino,
      file_type: st.st_mode & libc::S_IFMT,
      // Use only the access mode bits because other F_GETFL flags (like O_NONBLOCK/O_APPEND) are
      // mutable via F_SETFL and would make identity unstable.
      access_mode: flags & libc::O_ACCMODE,
    })
  }

  fn apply_changes(kqueue: RawFd, changes: &[libc::kevent]) -> io::Result<()> {
    if changes.is_empty() {
      return Ok(());
    }
    loop {
      let rc = unsafe {
        libc::kevent(
          kqueue,
          changes.as_ptr(),
          changes.len() as i32,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc != -1 {
        return Ok(());
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }

  fn changes_for_register(fd: RawFd, token: Token, interest: Interest) -> Vec<libc::kevent> {
    let mut changes = Vec::new();
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
    changes
  }

  fn changes_for_reregister(fd: RawFd, old: Registration, token: Token, interest: Interest) -> Vec<libc::kevent> {
    let mut changes = Vec::new();

    // Update / add / remove filters individually so we don't depend on kqueue "best effort" ENOENT
    // semantics, and so we can enforce mio-style register/reregister/deregister errors uniformly
    // across backends.
    for (filter, bit) in [
      (libc::EVFILT_READ, Interest::READABLE),
      (libc::EVFILT_WRITE, Interest::WRITABLE),
    ] {
      let had = old.interest.contains(bit);
      let wants = interest.contains(bit);

      match (had, wants) {
        (true, true) => {
          // Modify existing: ensure it's enabled and update udata (token).
          changes.push(make_kevent(
            fd,
            filter,
            token,
            libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
          ));
        }
        (false, true) => {
          // Newly interested: add+enable.
          changes.push(make_kevent(
            fd,
            filter,
            token,
            libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
          ));
        }
        (true, false) => {
          // No longer interested: delete.
          changes.push(make_kevent(fd, filter, old.token, libc::EV_DELETE));
        }
        (false, false) => {}
      }
    }

    changes
  }

  fn changes_for_deregister(fd: RawFd, reg: Registration) -> Vec<libc::kevent> {
    let mut changes = Vec::new();
    if reg.interest.contains(Interest::READABLE) {
      changes.push(make_kevent(fd, libc::EVFILT_READ, reg.token, libc::EV_DELETE));
    }
    if reg.interest.contains(Interest::WRITABLE) {
      changes.push(make_kevent(fd, libc::EVFILT_WRITE, reg.token, libc::EV_DELETE));
    }
    changes
  }

  fn register_user_waker(kqueue: RawFd, ident: libc::uintptr_t) -> io::Result<()> {
    let kev = libc::kevent {
      ident,
      filter: libc::EVFILT_USER,
      flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
      fflags: libc::NOTE_FFNOP,
      data: 0,
      udata: (Token::WAKE.0 as usize) as *mut libc::c_void,
    };
    loop {
      let rc = unsafe {
        libc::kevent(
          kqueue,
          &kev as *const libc::kevent,
          1,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc != -1 {
        return Ok(());
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }

  fn register_pipe_waker(kqueue: RawFd, read_fd: RawFd) -> io::Result<()> {
    let kev = libc::kevent {
      ident: read_fd as libc::uintptr_t,
      filter: libc::EVFILT_READ,
      flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
      fflags: 0,
      data: 0,
      udata: (Token::WAKE.0 as usize) as *mut libc::c_void,
    };
    loop {
      let rc = unsafe {
        libc::kevent(
          kqueue,
          &kev as *const libc::kevent,
          1,
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc != -1 {
        return Ok(());
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }

  fn is_evfilt_user_unsupported(err: &io::Error) -> bool {
    match err.raw_os_error() {
      Some(libc::ENOSYS)
      | Some(libc::EINVAL)
      | Some(libc::ENOTSUP)
      | Some(libc::EOPNOTSUPP)
      | Some(libc::EPERM) => true,
      _ => false,
    }
  }

  fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(any(
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "dragonfly"
    ))]
    {
      loop {
        let mut fds = [-1, -1];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if rc == 0 {
          // SAFETY: just created fds.
          return Ok((
            unsafe { OwnedFd::from_raw_fd(fds[0]) },
            unsafe { OwnedFd::from_raw_fd(fds[1]) },
          ));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        if err.raw_os_error() != Some(libc::ENOSYS) {
          return Err(err);
        }
        break;
      }
    }

    let (read_fd, write_fd) = loop {
      let mut fds = [-1, -1];
      let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
      if rc == 0 {
        break (fds[0], fds[1]);
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    };

    // SAFETY: just created fds.
    let read = unsafe { OwnedFd::from_raw_fd(read_fd) };
    let write = unsafe { OwnedFd::from_raw_fd(write_fd) };

    // Use fcntl to set flags when pipe2 isn't available.
    set_nonblocking(read.as_raw_fd())?;
    set_nonblocking(write.as_raw_fd())?;
    set_cloexec(read.as_raw_fd())?;
    set_cloexec(write.as_raw_fd())?;
    Ok((read, write))
  }

  fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = loop {
      let rc = unsafe { libc::fcntl(fd, libc::F_GETFL) };
      if rc != -1 {
        break rc;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    };
    loop {
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
      if rc != -1 {
        break;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
    Ok(())
  }

  fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = loop {
      let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
      if rc != -1 {
        break rc;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    };
    loop {
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
      if rc != -1 {
        break;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
    Ok(())
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
  use std::time::Duration;

  pub(super) struct ReactorSys;

  impl ReactorSys {
    pub(super) fn new_with_waker() -> io::Result<(ReactorSys, super::Waker)> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "runtime-native reactor is only supported on epoll/kqueue platforms",
      ))
    }

    pub(super) fn register(&self, _fd: RawFd, _token: Token, _interest: Interest) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn reregister(&self, _fd: RawFd, _token: Token, _interest: Interest) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn deregister(&self, _fd: RawFd) -> io::Result<()> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn poll_raw(&self, _timeout: Option<Duration>) -> io::Result<Vec<Event>> {
      Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported platform"))
    }

    pub(super) fn drain_waker(&self) -> io::Result<()> {
      Ok(())
    }
  }
}
