use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;

pub struct Epoll {
  fd: OwnedFd,
}

impl Epoll {
  pub fn new() -> io::Result<Self> {
    // SAFETY: syscall.
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is owned.
    Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
  }

  pub fn ctl_add(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    self.ctl(libc::EPOLL_CTL_ADD, fd, events, token)
  }

  #[allow(dead_code)]
  pub fn ctl_mod(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    self.ctl(libc::EPOLL_CTL_MOD, fd, events, token)
  }

  pub fn ctl_del(&self, fd: RawFd) -> io::Result<()> {
    // Per `epoll_ctl(2)`, the `event` argument can be null for DEL.
    let rc = unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  }

  fn ctl(&self, op: libc::c_int, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut ev: libc::epoll_event = unsafe { mem::zeroed() };
    ev.events = events;
    ev.u64 = token;
    let rc = unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), op, fd, &mut ev) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  }

  pub fn wait(&self, events: &mut [libc::epoll_event], timeout_ms: i32) -> io::Result<usize> {
    debug_assert!(timeout_ms >= -1);

    let rc = unsafe {
      libc::epoll_wait(
        self.fd.as_raw_fd(),
        events.as_mut_ptr(),
        events.len().try_into().unwrap_or(i32::MAX),
        timeout_ms,
      )
    };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(rc as usize)
  }
}

pub struct EventFd {
  fd: OwnedFd,
}

impl EventFd {
  pub fn new() -> io::Result<Self> {
    // SAFETY: syscall.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is owned.
    Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
  }

  pub fn as_raw_fd(&self) -> RawFd {
    self.fd.as_raw_fd()
  }

  pub fn wake(&self) {
    let buf: u64 = 1;
    // SAFETY: syscall. Ignore EAGAIN (counter saturated) and EINTR.
    let _ = unsafe { libc::write(self.fd.as_raw_fd(), (&buf as *const u64).cast::<libc::c_void>(), 8) };
  }

  pub fn drain(&self) -> io::Result<()> {
    let mut buf: u64 = 0;
    loop {
      // SAFETY: syscall.
      let rc = unsafe { libc::read(self.fd.as_raw_fd(), (&mut buf as *mut u64).cast::<libc::c_void>(), 8) };
      if rc == 8 {
        continue;
      }
      if rc < 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
          Some(libc::EAGAIN) => return Ok(()),
          Some(libc::EINTR) => continue,
          _ => return Err(err),
        }
      }
      // rc == 0 shouldn't happen for eventfd; treat as drained.
      return Ok(());
    }
  }
}
