#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

#[cfg(target_os = "linux")]
mod timeout;

#[cfg(target_os = "linux")]
mod linux {
  use std::any::Any;
  use std::collections::HashMap;
  use std::collections::VecDeque;
  use std::ffi::CString;
  use std::io;
  use std::mem::MaybeUninit;
  use std::os::unix::io::RawFd;
  use std::path::Path;
  use std::time::Duration;

  use io_uring::opcode;
  use io_uring::squeue;
  use io_uring::types;
  use io_uring::IoUring;

  use crate::timeout::duration_to_timespec;

  fn cstring_from_path(path: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "path contains an interior NUL byte",
      )
    })
  }

  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
  pub struct OpId(u64);

  impl OpId {
    fn as_u64(self) -> u64 {
      self.0
    }
  }

  #[derive(Debug)]
  pub struct OpWithTimeout {
    pub op_id: OpId,
    pub timeout_id: OpId,
  }

  #[derive(Debug)]
  pub enum PreparedOp {
    Read {
      fd: RawFd,
      buf: Vec<u8>,
      keep_alive: Option<Box<dyn Any + Send + Sync>>,
    },
    OpenAt {
      dirfd: RawFd,
      path: CString,
      flags: i32,
      mode: u32,
      keep_alive: Option<Box<dyn Any + Send + Sync>>,
    },
    Statx {
      dirfd: RawFd,
      path: CString,
      flags: i32,
      mask: u32,
      out: Box<MaybeUninit<libc::statx>>,
      keep_alive: Option<Box<dyn Any + Send + Sync>>,
    },
  }

  impl PreparedOp {
    pub fn read(fd: RawFd, buf: Vec<u8>) -> Self {
      Self::Read {
        fd,
        buf,
        keep_alive: None,
      }
    }

    pub fn read_with_keep_alive(
      fd: RawFd,
      buf: Vec<u8>,
      keep_alive: impl Any + Send + Sync,
    ) -> Self {
      Self::Read {
        fd,
        buf,
        keep_alive: Some(Box::new(keep_alive)),
      }
    }

    pub fn openat(dirfd: RawFd, path: &Path, flags: i32, mode: u32) -> io::Result<Self> {
      Ok(Self::OpenAt {
        dirfd,
        path: cstring_from_path(path)?,
        flags,
        mode,
        keep_alive: None,
      })
    }

    pub fn openat_with_keep_alive(
      dirfd: RawFd,
      path: &Path,
      flags: i32,
      mode: u32,
      keep_alive: impl Any + Send + Sync,
    ) -> io::Result<Self> {
      Ok(Self::OpenAt {
        dirfd,
        path: cstring_from_path(path)?,
        flags,
        mode,
        keep_alive: Some(Box::new(keep_alive)),
      })
    }

    pub fn statx(dirfd: RawFd, path: &Path, flags: i32, mask: u32) -> io::Result<Self> {
      Ok(Self::Statx {
        dirfd,
        path: cstring_from_path(path)?,
        flags,
        mask,
        out: Box::new(MaybeUninit::uninit()),
        keep_alive: None,
      })
    }

    pub fn statx_with_keep_alive(
      dirfd: RawFd,
      path: &Path,
      flags: i32,
      mask: u32,
      keep_alive: impl Any + Send + Sync,
    ) -> io::Result<Self> {
      Ok(Self::Statx {
        dirfd,
        path: cstring_from_path(path)?,
        flags,
        mask,
        out: Box::new(MaybeUninit::uninit()),
        keep_alive: Some(Box::new(keep_alive)),
      })
    }

    pub fn into_statx_result(self, res: i32) -> io::Result<libc::statx> {
      match self {
        PreparedOp::Statx { out, .. } => {
          if res == 0 {
            Ok(unsafe { (*out).assume_init() })
          } else if res < 0 {
            Err(io::Error::from_raw_os_error(-res))
          } else {
            Err(io::Error::new(
              io::ErrorKind::Other,
              "statx returned an unexpected positive result",
            ))
          }
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "not a statx operation",
        )),
      }
    }

    fn build_sqe(&mut self) -> squeue::Entry {
      match self {
        PreparedOp::Read { fd, buf, .. } => {
          opcode::Read::new(types::Fd(*fd), buf.as_mut_ptr(), buf.len() as _).build()
        }
        PreparedOp::OpenAt {
          dirfd,
          path,
          flags,
          mode,
          ..
        } => opcode::OpenAt::new(types::Fd(*dirfd), path.as_ptr())
          .flags(*flags)
          .mode(*mode)
          .build(),
        PreparedOp::Statx {
          dirfd,
          path,
          flags,
          mask,
          out,
          ..
        } => {
          let out_ptr = out.as_mut().as_mut_ptr() as *mut types::statx;
          opcode::Statx::new(types::Fd(*dirfd), path.as_ptr(), out_ptr)
            .flags(*flags)
            .mask(*mask)
            .build()
        }
      }
    }
  }

  #[derive(Debug)]
  pub enum Completion {
    Op { id: OpId, res: i32, op: PreparedOp },
    Timeout { id: OpId, target: OpId, res: i32 },
    Cancel { id: OpId, target: OpId, res: i32 },
  }

  enum OpState {
    Target { op: PreparedOp },
    Timeout { target: OpId, _ts: Box<types::Timespec> },
    Cancel { target: OpId },
  }

  pub struct Driver {
    ring: IoUring,
    next_id: u64,
    ops: HashMap<OpId, OpState>,
    ready: VecDeque<Completion>,
  }

  impl Driver {
    fn submission_queue_full() -> io::Error {
      io::Error::new(io::ErrorKind::Other, "io_uring submission queue is full")
    }

    pub fn new(entries: u32) -> io::Result<Self> {
      Ok(Self {
        ring: IoUring::new(entries)?,
        next_id: 1,
        ops: HashMap::new(),
        ready: VecDeque::new(),
      })
    }

    fn alloc_id(&mut self) -> OpId {
      let id = OpId(self.next_id);
      self.next_id = self.next_id.wrapping_add(1);
      id
    }

    pub fn submit(&mut self, mut op: PreparedOp) -> io::Result<OpId> {
      let op_id = self.alloc_id();

      let entry = op.build_sqe().user_data(op_id.as_u64());
      {
        let mut sq = self.ring.submission();
        let available = sq.capacity() - sq.len();
        if available < 1 {
          return Err(Self::submission_queue_full());
        }

        self.ops.insert(op_id, OpState::Target { op });

        unsafe {
          sq.push(&entry).unwrap();
        }
      }
      self.ring.submit()?;

      Ok(op_id)
    }

    pub fn submit_openat(&mut self, dirfd: RawFd, path: &Path, flags: i32, mode: u32) -> io::Result<OpId> {
      self.submit(PreparedOp::openat(dirfd, path, flags, mode)?)
    }

    pub fn submit_statx(&mut self, dirfd: RawFd, path: &Path, flags: i32, mask: u32) -> io::Result<OpId> {
      self.submit(PreparedOp::statx(dirfd, path, flags, mask)?)
    }

    /// Submit `op` with a per-operation timeout.
    ///
    /// This uses `IOSQE_IO_LINK` + `IORING_OP_LINK_TIMEOUT`:
    /// - If the target op completes before the timeout, the target CQE returns its normal result
    ///   (>= 0 for success, `-errno` for failure) and the timeout CQE returns `-ECANCELED`.
    /// - If the timeout expires first, the timeout CQE returns `-ETIME` and the target op CQE
    ///   returns `-ECANCELED`.
    ///
    /// Resource lifetime rule: even if the timeout CQE is observed first, resources owned by the
    /// target op (buffers, pins, roots) are not released until the target op CQE is processed.
    pub fn submit_with_timeout(&mut self, mut op: PreparedOp, timeout: Duration) -> io::Result<OpWithTimeout> {
      let op_id = self.alloc_id();
      let timeout_id = self.alloc_id();

      let entry = op
        .build_sqe()
        .flags(squeue::Flags::IO_LINK)
        .user_data(op_id.as_u64());

      // `IORING_OP_LINK_TIMEOUT` takes a pointer to a `__kernel_timespec`. That memory must stay
      // valid until the timeout request completes, so it cannot live on the stack here.
      let ts = Box::new(duration_to_timespec(timeout));
      let timeout_entry = opcode::LinkTimeout::new(&*ts)
        .build()
        .user_data(timeout_id.as_u64());

      {
        let mut sq = self.ring.submission();
        let available = sq.capacity() - sq.len();
        if available < 2 {
          return Err(Self::submission_queue_full());
        }

        self.ops.insert(op_id, OpState::Target { op });
        self.ops.insert(timeout_id, OpState::Timeout { target: op_id, _ts: ts });

        unsafe {
          sq.push(&entry).unwrap();
          sq.push(&timeout_entry).unwrap();
        }
      }
      self.ring.submit()?;

      Ok(OpWithTimeout { op_id, timeout_id })
    }

    /// Submit an async cancellation request for `target`.
    ///
    /// The returned [`OpId`] refers to the cancel request itself. The cancel CQE result is:
    /// - `0` if one request was successfully canceled
    /// - `-ENOENT` if the target request could not be found (already completed/canceled)
    ///
    /// The target op still delivers its own CQE, typically `-ECANCELED` if the cancellation won.
    pub fn cancel(&mut self, target: OpId) -> io::Result<OpId> {
      let cancel_id = self.alloc_id();

      let entry = opcode::AsyncCancel::new(target.as_u64()).build().user_data(cancel_id.as_u64());

      {
        let mut sq = self.ring.submission();
        let available = sq.capacity() - sq.len();
        if available < 1 {
          return Err(Self::submission_queue_full());
        }

        self.ops.insert(cancel_id, OpState::Cancel { target });

        unsafe {
          sq.push(&entry).unwrap();
        }
      }
      self.ring.submit()?;

      Ok(cancel_id)
    }

    pub fn wait(&mut self) -> io::Result<Completion> {
      loop {
        if let Some(c) = self.ready.pop_front() {
          return Ok(c);
        }

        self.ring.submit_and_wait(1)?;

        let cq = self.ring.completion();
        for cqe in cq {
          let id = OpId(cqe.user_data());
          let res = cqe.result();

          let state = match self.ops.remove(&id) {
            Some(state) => state,
            None => continue,
          };

          match state {
            OpState::Target { op } => self.ready.push_back(Completion::Op { id, res, op }),
            OpState::Timeout { target, .. } => self.ready.push_back(Completion::Timeout { id, target, res }),
            OpState::Cancel { target } => self.ready.push_back(Completion::Cancel { id, target, res }),
          }
        }
      }
    }
  }

  pub fn is_link_timeout_supported(driver: &Driver) -> io::Result<bool> {
    let mut probe = io_uring::Probe::new();
    driver.ring.submitter().register_probe(&mut probe)?;
    Ok(probe.is_supported(opcode::LinkTimeout::CODE))
  }

  pub fn is_async_cancel_supported(driver: &Driver) -> io::Result<bool> {
    let mut probe = io_uring::Probe::new();
    driver.ring.submitter().register_probe(&mut probe)?;
    Ok(probe.is_supported(opcode::AsyncCancel::CODE))
  }
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux {
  use std::io;
  use std::time::Duration;

  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
  pub struct OpId(u64);

  #[derive(Debug)]
  pub struct OpWithTimeout {
    pub op_id: OpId,
    pub timeout_id: OpId,
  }

  #[derive(Debug)]
  pub enum PreparedOp {}

  #[derive(Debug)]
  pub enum Completion {}

  pub struct Driver;

  impl Driver {
    pub fn new(_entries: u32) -> io::Result<Self> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring is only supported on Linux",
      ))
    }

    pub fn submit(&mut self, _op: PreparedOp) -> io::Result<OpId> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring is only supported on Linux",
      ))
    }

    pub fn submit_with_timeout(&mut self, _op: PreparedOp, _timeout: Duration) -> io::Result<OpWithTimeout> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring is only supported on Linux",
      ))
    }

    pub fn cancel(&mut self, _target: OpId) -> io::Result<OpId> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring is only supported on Linux",
      ))
    }

    pub fn wait(&mut self) -> io::Result<Completion> {
      Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "io_uring is only supported on Linux",
      ))
    }
  }

  pub fn is_link_timeout_supported(_driver: &Driver) -> io::Result<bool> {
    Ok(false)
  }

  pub fn is_async_cancel_supported(_driver: &Driver) -> io::Result<bool> {
    Ok(false)
  }
}

#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
