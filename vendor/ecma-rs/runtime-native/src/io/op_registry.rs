use crate::abi::PromiseRef;
use crate::roots;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use super::op::IoOp as PinnedIoOp;

/// Identifier for a runtime-native async I/O operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IoOpId(u64);

impl IoOpId {
  #[inline]
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOpOutcome {
  Ok(usize),
  Err(i32),
  Canceled,
}

#[derive(Debug)]
pub struct RootPin {
  handle: u32,
}

impl RootPin {
  pub fn new(ptr: *mut u8) -> Self {
    Self {
      handle: roots::global_root_registry().pin(ptr),
    }
  }
}

impl Drop for RootPin {
  fn drop(&mut self) {
    roots::global_root_registry().unregister(self.handle);
  }
}

/// Test-only synchronization hooks.
#[derive(Clone, Debug)]
pub struct IoOpDebugHooks {
  reached_finish: Arc<AtomicBool>,
  finish_barrier: Arc<Barrier>,
}

impl IoOpDebugHooks {
  pub fn pause_before_finish() -> Self {
    Self {
      reached_finish: Arc::new(AtomicBool::new(false)),
      finish_barrier: Arc::new(Barrier::new(2)),
    }
  }

  pub fn reached_finish(&self) -> bool {
    self.reached_finish.load(Ordering::Acquire)
  }

  pub fn release_finish(&self) {
    self.finish_barrier.wait();
  }

  pub(crate) fn pause_finish_now(&self) {
    self.reached_finish.store(true, Ordering::Release);
    self.finish_barrier.wait();
  }
}

#[derive(Debug)]
pub struct CancellationToken {
  inner: Arc<CancellationInner>,
}

#[derive(Debug)]
struct CancellationInner {
  cancelled: AtomicBool,
  read_fd: OwnedFd,
  write_fd: OwnedFd,
}

// Safety: the cancellation token owns two file descriptors that are only accessed via syscalls.
unsafe impl Send for CancellationInner {}
unsafe impl Sync for CancellationInner {}

impl CancellationToken {
  pub fn new() -> io::Result<Self> {
    let (read_fd, write_fd) = cancel_pipe()?;
    Ok(Self {
      inner: Arc::new(CancellationInner {
        cancelled: AtomicBool::new(false),
        read_fd,
        write_fd,
      }),
    })
  }

  #[inline]
  pub fn is_cancelled(&self) -> bool {
    self.inner.cancelled.load(Ordering::Acquire)
  }

  pub fn cancel(&self) {
    if self.inner.cancelled.swap(true, Ordering::AcqRel) {
      return;
    }

    let byte = [1u8; 1];
    unsafe {
      let _ = libc::write(
        self.inner.write_fd.as_raw_fd(),
        byte.as_ptr() as *const libc::c_void,
        1,
      );
    }
  }

  pub fn poll_fd(&self) -> RawFd {
    self.inner.read_fd.as_raw_fd()
  }

  pub fn drain(&self) {
    let mut buf = [0u8; 64];
    loop {
      let rc = unsafe {
        libc::read(
          self.inner.read_fd.as_raw_fd(),
          buf.as_mut_ptr() as *mut libc::c_void,
          buf.len(),
        )
      };
      if rc > 0 {
        continue;
      }
      if rc == 0 {
        break;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::WouldBlock {
        break;
      }
      break;
    }
  }
}

impl Clone for CancellationToken {
  fn clone(&self) -> Self {
    Self {
      inner: Arc::clone(&self.inner),
    }
  }
}

#[derive(Debug)]
pub enum IoOpKind {
  Write { fd: OwnedFd },
}

impl IoOpKind {
  pub fn raw_fd(&self) -> RawFd {
    match self {
      IoOpKind::Write { fd } => fd.as_raw_fd(),
    }
  }

  pub fn poll_events(&self) -> i16 {
    match self {
      IoOpKind::Write { .. } => libc::POLLOUT,
    }
  }
}

/// An in-flight I/O operation record stored in the runtime registry.
///
/// This owns:
/// - the pinned backing stores + accounting permit (`pinned`)
/// - GC root pins required to settle/reject the associated promise
/// - the cancellation token
pub(crate) struct IoOpRecord {
  id: IoOpId,
  pub(crate) kind: IoOpKind,
  pub(crate) promise: PromiseRef,
  pub(crate) cancel: CancellationToken,
  pub(crate) roots: Vec<RootPin>,
  /// Pinned backing store + accounting permit.
  ///
  /// NOTE: Keep this *after* `roots` so dropping an op cannot observe
  /// `inflight_ops_current == 0` while root pins are still registered.
  pub(crate) pinned: PinnedIoOp,
  pub(crate) debug: Option<IoOpDebugHooks>,
  outcome: Mutex<Option<IoOpOutcome>>,
}

// Safety: `IoOpRecord` contains raw pointers (inside `PinnedIoOp`'s `IoBuf`) and file descriptors.
// The runtime pins buffers for the lifetime of the op record and serializes access to the fds.
unsafe impl Send for IoOpRecord {}
unsafe impl Sync for IoOpRecord {}

impl IoOpRecord {
  pub(crate) fn new(
    id: IoOpId,
    kind: IoOpKind,
    pinned: PinnedIoOp,
    promise: PromiseRef,
    cancel: CancellationToken,
    roots: Vec<RootPin>,
    debug: Option<IoOpDebugHooks>,
  ) -> Self {
    Self {
      id,
      kind,
      promise,
      cancel,
      roots,
      pinned,
      debug,
      outcome: Mutex::new(None),
    }
  }

  pub fn id(&self) -> IoOpId {
    self.id
  }

  pub fn set_outcome(&self, out: IoOpOutcome) {
    *self.outcome.lock() = Some(out);
  }

  pub fn take_outcome(&self) -> Option<IoOpOutcome> {
    self.outcome.lock().take()
  }
}

impl Drop for IoOpRecord {
  fn drop(&mut self) {
    // Ensure any global-root pins held for the duration of the I/O op are released
    // as soon as the op record is dropped.
    //
    // This avoids observable intermediate states where the I/O limiter counters
    // have reached 0 but GC pins are still present (teardown tests expect pins
    // to be released promptly once the last op record reference is dropped).
    self.roots.clear();
  }
}

pub(crate) struct OpRegistry {
  next_id: u64,
  ops: HashMap<IoOpId, Arc<IoOpRecord>>,
}

impl Default for OpRegistry {
  fn default() -> Self {
    Self::new()
  }
}

impl OpRegistry {
  pub fn new() -> Self {
    Self {
      next_id: 1,
      ops: HashMap::new(),
    }
  }

  pub fn alloc_id(&mut self) -> IoOpId {
    loop {
      let id = self.next_id;
      self.next_id = self.next_id.wrapping_add(1);
      if id != 0 {
        return IoOpId(id);
      }
    }
  }

  pub fn insert(&mut self, op: Arc<IoOpRecord>) {
    self.ops.insert(op.id, op);
  }

  pub fn remove(&mut self, id: IoOpId) -> Option<Arc<IoOpRecord>> {
    self.ops.remove(&id)
  }

  pub fn len(&self) -> usize {
    self.ops.len()
  }

  pub fn drain(&mut self) -> Vec<Arc<IoOpRecord>> {
    self.ops.drain().map(|(_k, v)| v).collect()
  }
}

#[cfg(unix)]
fn cancel_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  unsafe {
    let mut fds = [0i32; 2];
    if libc::pipe(fds.as_mut_ptr()) != 0 {
      return Err(io::Error::last_os_error());
    }
    set_nonblocking(fds[0])?;
    set_nonblocking(fds[1])?;
    Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
  }
}

#[cfg(not(unix))]
fn cancel_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "I/O cancellation pipes are only supported on unix platforms",
  ))
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    if flags < 0 {
      return Err(io::Error::last_os_error());
    }
    if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
      return Err(io::Error::last_os_error());
    }
  }
  Ok(())
}
