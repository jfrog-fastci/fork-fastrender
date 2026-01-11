use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct Kqueue {
  fd: OwnedFd,
}

impl Kqueue {
  pub fn new() -> io::Result<Self> {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `kqueue()` returns an owned fd on success.
    let fd = unsafe { OwnedFd::from_raw_fd(kq) };
    set_cloexec(fd.as_raw_fd())?;
    Ok(Self { fd })
  }

  #[inline]
  pub fn as_raw_fd(&self) -> RawFd {
    self.fd.as_raw_fd()
  }

  pub fn ctl(&self, changes: &[libc::kevent]) -> io::Result<()> {
    if changes.is_empty() {
      return Ok(());
    }

    loop {
      let rc = unsafe {
        libc::kevent(
          self.fd.as_raw_fd(),
          changes.as_ptr(),
          changes.len().try_into().unwrap_or(i32::MAX),
          std::ptr::null_mut(),
          0,
          std::ptr::null(),
        )
      };
      if rc >= 0 {
        return Ok(());
      }

      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }

  pub fn wait(&self, events: &mut [libc::kevent], timeout_ms: i32) -> io::Result<usize> {
    debug_assert!(timeout_ms >= -1);

    let mut ts_storage = libc::timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    let ts_ptr = if timeout_ms < 0 {
      std::ptr::null()
    } else {
      ts_storage.tv_sec = (timeout_ms as i64 / 1000) as libc::time_t;
      ts_storage.tv_nsec = ((timeout_ms as i64 % 1000) * 1_000_000) as libc::c_long;
      &ts_storage as *const libc::timespec
    };

    loop {
      let rc = unsafe {
        libc::kevent(
          self.fd.as_raw_fd(),
          std::ptr::null(),
          0,
          events.as_mut_ptr(),
          events.len().try_into().unwrap_or(i32::MAX),
          ts_ptr,
        )
      };
      if rc >= 0 {
        return Ok(rc as usize);
      }

      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(err);
    }
  }
}

pub enum Waker {
  User {
    kqueue: RawFd,
    ident: libc::uintptr_t,
    token: usize,
  },
  Pipe {
    read: OwnedFd,
    write: OwnedFd,
  },
}

impl Waker {
  pub fn new(kqueue: &Kqueue, token: u64) -> io::Result<Self> {
    if usize::BITS < 64 && token > usize::MAX as u64 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "kqueue udata cannot represent 64-bit token on this platform",
      ));
    }
    let token_usize = token as usize;

    if cfg!(feature = "force_pipe_wake") {
      return Self::new_pipe(kqueue, token_usize);
    }

    // Preferred wake mechanism: EVFILT_USER.
    let ident: libc::uintptr_t = 1;
    let user_res = register_user_waker(kqueue.as_raw_fd(), ident, token_usize);
    match user_res {
      Ok(()) => Ok(Waker::User {
        kqueue: kqueue.as_raw_fd(),
        ident,
        token: token_usize,
      }),
      Err(err) if is_evfilt_user_unsupported(&err) => Self::new_pipe(kqueue, token_usize),
      Err(err) => Err(err),
    }
  }

  fn new_pipe(kqueue: &Kqueue, token: usize) -> io::Result<Self> {
    let (read, write) = create_pipe()?;
    register_pipe_waker(kqueue.as_raw_fd(), read.as_raw_fd(), token)?;
    Ok(Waker::Pipe { read, write })
  }

  pub fn wake(&self) {
    match self {
      Waker::User { kqueue, ident, token } => loop {
        let kev = libc::kevent {
          ident: *ident,
          filter: libc::EVFILT_USER,
          flags: 0,
          fflags: libc::NOTE_TRIGGER,
          data: 0,
          udata: (*token as usize) as *mut libc::c_void,
        };
        let rc = unsafe {
          libc::kevent(
            *kqueue,
            &kev as *const libc::kevent,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
          )
        };
        if rc >= 0 {
          return;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        // Best-effort wake. The async runtime uses wake for liveness; avoid panicking.
        return;
      },
      Waker::Pipe { write, .. } => loop {
        let buf = [0u8; 1];
        let rc = unsafe { libc::write(write.as_raw_fd(), buf.as_ptr() as *const libc::c_void, 1) };
        if rc == 1 {
          return;
        }
        if rc < 0 {
          let err = io::Error::last_os_error();
          match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return,
            _ => return,
          }
        }
        // Treat unexpected short writes as a no-op.
        return;
      },
    }
  }

  pub fn drain(&self) -> io::Result<()> {
    match self {
      // EVFILT_USER is registered with EV_CLEAR, so retrieving the event clears it.
      Waker::User { .. } => Ok(()),
      Waker::Pipe { read, .. } => {
        let mut buf = [0u8; 256];
        loop {
          let rc = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
          if rc > 0 {
            continue;
          }
          if rc == 0 {
            return Ok(());
          }
          let err = io::Error::last_os_error();
          match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(err),
          }
        }
      }
    }
  }
}

fn register_user_waker(kqueue: RawFd, ident: libc::uintptr_t, token: usize) -> io::Result<()> {
  let kev = libc::kevent {
    ident,
    filter: libc::EVFILT_USER,
    flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
    fflags: libc::NOTE_FFNOP,
    data: 0,
    udata: (token as usize) as *mut libc::c_void,
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
    if rc >= 0 {
      return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  }
}

fn register_pipe_waker(kqueue: RawFd, read_fd: RawFd, token: usize) -> io::Result<()> {
  let kev = libc::kevent {
    ident: read_fd as libc::uintptr_t,
    filter: libc::EVFILT_READ,
    flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
    fflags: 0,
    data: 0,
    udata: (token as usize) as *mut libc::c_void,
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
    if rc >= 0 {
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
  // Fast path: some BSDs support `pipe2` (atomic O_NONBLOCK|O_CLOEXEC).
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
        // SAFETY: `pipe2` returns new, owned fds on success.
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

  // SAFETY: `pipe` returns new, owned fds.
  let read = unsafe { OwnedFd::from_raw_fd(read_fd) };
  let write = unsafe { OwnedFd::from_raw_fd(write_fd) };

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

pub fn make_kevent(fd: RawFd, filter: i16, token: u64, flags: u16) -> libc::kevent {
  libc::kevent {
    ident: fd as libc::uintptr_t,
    filter,
    flags,
    fflags: 0,
    data: 0,
    udata: (token as usize) as *mut libc::c_void,
  }
}

pub fn make_kevent_user(ident: libc::uintptr_t, token: u64, flags: u16, fflags: u32) -> libc::kevent {
  libc::kevent {
    ident,
    filter: libc::EVFILT_USER,
    flags,
    fflags,
    data: 0,
    udata: (token as usize) as *mut libc::c_void,
  }
}

pub fn kevent_token(kev: &libc::kevent) -> u64 {
  kev.udata as usize as u64
}
