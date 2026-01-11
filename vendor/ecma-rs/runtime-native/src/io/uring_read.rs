#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

#[cfg(target_os = "linux")]
use io_uring::opcode;
#[cfg(target_os = "linux")]
use io_uring::types;

use crate::buffer::BorrowGuardWrite;
use crate::buffer::typed_array::{PinnedUint8Array, TypedArrayError, Uint8Array};
use crate::gc::{GcHeap, RootHandle, OBJ_HEADER_SIZE};

#[derive(Debug, thiserror::Error)]
pub enum IoError {
  #[error("operation cancelled")]
  Cancelled,
  #[error("io_uring: {0}")]
  Uring(i32),
  #[error("invalid I/O buffer: {0:?}")]
  InvalidBuffer(TypedArrayError),
  #[error("I/O buffer is in use by another in-flight operation")]
  BufferBorrowed,
}

#[derive(Clone, Debug)]
pub struct CancellationToken {
  inner: Arc<CancellationTokenInner>,
}

#[derive(Debug)]
struct CancellationTokenInner {
  cancelled: AtomicBool,
  waker: Mutex<Option<Waker>>,
}

impl CancellationToken {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(CancellationTokenInner {
        cancelled: AtomicBool::new(false),
        waker: Mutex::new(None),
      }),
    }
  }

  pub fn cancel(&self) {
    self.inner.cancelled.store(true, Ordering::Release);
    if let Some(waker) = self
      .inner
      .waker
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take()
    {
      waker.wake();
    }
  }

  pub fn is_cancelled(&self) -> bool {
    self.inner.cancelled.load(Ordering::Acquire)
  }

  pub(crate) fn register(&self, waker: &Waker) {
    let mut lock = self
      .inner
      .waker
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *lock = Some(waker.clone());
    // If cancellation raced with registration, wake so the future polls again and can request
    // io_uring cancellation.
    if self.is_cancelled() {
      if let Some(w) = lock.take() {
        w.wake();
      }
    }
  }
}

pub struct IoOp {
  id: u64,
  buf: Mutex<Option<PinnedUint8Array>>,
  borrow: Mutex<Option<BorrowGuardWrite>>,
  ptr: *mut u8,
  len: usize,

  heap: Arc<Mutex<GcHeap>>,
  buffer_root: RootHandle,
  promise_root: RootHandle,

  cancel_requested: AtomicBool,
  completion_started: AtomicBool,
  result: Mutex<Option<Result<usize, IoError>>>,
  waker: Mutex<Option<Waker>>,
}

unsafe impl Send for IoOp {}
unsafe impl Sync for IoOp {}

impl std::fmt::Debug for IoOp {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IoOp")
      .field("id", &self.id)
      .field("ptr", &self.ptr)
      .field("len", &self.len)
      .field("buffer_root", &self.buffer_root)
      .field("promise_root", &self.promise_root)
      .field(
        "cancel_requested",
        &self.cancel_requested.load(Ordering::Relaxed),
      )
      .field(
        "completion_started",
        &self.completion_started.load(Ordering::Relaxed),
      )
      .finish_non_exhaustive()
  }
}

impl IoOp {
  fn new(
    id: u64,
    heap: Arc<Mutex<GcHeap>>,
    buffer_obj: *mut u8,
    promise_obj: *mut u8,
    buf: PinnedUint8Array,
    borrow: BorrowGuardWrite,
  ) -> Self {
    let (buffer_root, promise_root) = {
      let mut heap_lock = heap.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      (heap_lock.root_add(buffer_obj), heap_lock.root_add(promise_obj))
    };

    let ptr = buf.as_ptr();
    let len = buf.len();

    Self {
      id,
      buf: Mutex::new(Some(buf)),
      borrow: Mutex::new(Some(borrow)),
      ptr,
      len,
      heap,
      buffer_root,
      promise_root,
      cancel_requested: AtomicBool::new(false),
      completion_started: AtomicBool::new(false),
      result: Mutex::new(None),
      waker: Mutex::new(None),
    }
  }

  fn cancel_requested(&self) -> bool {
    self.cancel_requested.load(Ordering::Acquire)
  }

  fn mark_cancel_requested(&self) -> bool {
    !self.cancel_requested.swap(true, Ordering::AcqRel)
  }

  fn complete(&self, res: Result<usize, IoError>) {
    // Ensure we resolve exactly once.
    if self.completion_started.swap(true, Ordering::AcqRel) {
      return;
    }

    // Drop the I/O op's persistent roots and release the backing-store pin.
    {
      let mut heap_lock = self.heap.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      heap_lock.root_remove(self.buffer_root);
      heap_lock.root_remove(self.promise_root);
    }

    self
      .buf
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take();
    self
      .borrow
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take();

    {
      let mut lock = self.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = Some(res);
    }

    if let Some(waker) = self
      .waker
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take()
    {
      waker.wake();
    }
  }

  fn take_result(&self) -> Option<Result<usize, IoError>> {
    let mut lock = self.result.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.take()
  }
}

#[derive(Debug)]
pub struct ReadFuture {
  op: Arc<IoOp>,
  driver: UringDriver,
  cancel: Option<CancellationToken>,
}

impl Future for ReadFuture {
  type Output = Result<usize, IoError>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.get_mut();

    if let Some(res) = this.op.take_result() {
      return Poll::Ready(res);
    }

    {
      let mut lock = this.op.waker.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = Some(cx.waker().clone());
    }

    if let Some(token) = &this.cancel {
      if token.is_cancelled() {
        if this.op.mark_cancel_requested() {
          this.driver.request_cancel(this.op.id);
        }
      } else {
        token.register(cx.waker());
      }
    }

    Poll::Pending
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserDataKind {
  Read = 0,
  Cancel = 1,
}

#[inline]
fn user_data(id: u64, kind: UserDataKind) -> u64 {
  (id << 1) | (kind as u64)
}

#[inline]
fn decode_user_data_kind(user_data: u64) -> UserDataKind {
  match user_data & 1 {
    0 => UserDataKind::Read,
    _ => UserDataKind::Cancel,
  }
}

#[inline]
fn decode_user_data_id(user_data: u64) -> u64 {
  user_data >> 1
}

enum Command {
  SubmitRead { op: Arc<IoOp>, fd: RawFd },
  Cancel { id: u64 },
  Shutdown,
}

#[derive(Debug)]
struct DriverInner {
  cmd_tx: mpsc::Sender<Command>,
  wake_fd: RawFd,
  next_id: AtomicU64,
}

impl Drop for DriverInner {
  fn drop(&mut self) {
    let _ = self.cmd_tx.send(Command::Shutdown);
    let val: u64 = 1;
    let buf = val.to_ne_bytes();
    unsafe {
      libc::write(self.wake_fd, buf.as_ptr() as *const libc::c_void, buf.len());
    }
  }
}

/// Minimal io_uring driver used by the runtime-native I/O bridge layer.
///
/// This is intentionally narrow in scope: it exists to safely submit kernel operations that write
/// directly into a GC-managed `Uint8Array` backing store, while keeping the JS values rooted until
/// completion.
#[derive(Clone, Debug)]
pub struct UringDriver {
  inner: Arc<DriverInner>,
}

impl UringDriver {
  pub fn new(entries: u32) -> Result<Self, std::io::Error> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake_fd < 0 {
      return Err(std::io::Error::last_os_error());
    }

    #[cfg(target_os = "linux")]
    thread::spawn(move || run_driver(entries, wake_fd, cmd_rx));
    #[cfg(not(target_os = "linux"))]
    thread::spawn(move || {
      let _ = entries;
      let _ = wake_fd;
      let _ = cmd_rx;
    });

    Ok(Self {
      inner: Arc::new(DriverInner {
        cmd_tx,
        wake_fd,
        next_id: AtomicU64::new(1),
      }),
    })
  }

  fn next_op_id(&self) -> u64 {
    self.inner.next_id.fetch_add(1, Ordering::Relaxed)
  }

  fn request_cancel(&self, id: u64) {
    let _ = self.inner.cmd_tx.send(Command::Cancel { id });
    self.signal();
  }

  fn signal(&self) {
    let val: u64 = 1;
    let buf = val.to_ne_bytes();
    unsafe {
      libc::write(self.inner.wake_fd, buf.as_ptr() as *const libc::c_void, buf.len());
    }
  }

  /// Submit a read into a GC-managed `Uint8Array` object.
  ///
  /// `array_obj` and `promise_obj` are GC object **base pointers** (start of `ObjHeader`).
  pub fn read_into_uint8_array(
    &self,
    heap: Arc<Mutex<GcHeap>>,
    fd: RawFd,
    array_obj: *mut u8,
    promise_obj: *mut u8,
    cancel: Option<CancellationToken>,
  ) -> Result<ReadFuture, IoError> {
    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
      return Err(IoError::Cancelled);
    }

    // Safety: callers promise `array_obj` points to a `Uint8Array` object payload after ObjHeader.
    let view = unsafe { &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array) };
    let pinned = view.pin().map_err(IoError::InvalidBuffer)?;
    let borrow = pinned
      .backing_store()
      .try_borrow_io_write()
      .map_err(|_| IoError::BufferBorrowed)?;

    let id = self.next_op_id();
    let op = Arc::new(IoOp::new(id, heap, array_obj, promise_obj, pinned, borrow));

    let _ = self.inner.cmd_tx.send(Command::SubmitRead { op: Arc::clone(&op), fd });
    self.signal();

    Ok(ReadFuture {
      op,
      driver: self.clone(),
      cancel,
    })
  }
}

#[cfg(target_os = "linux")]
fn run_driver(entries: u32, wake_fd: RawFd, cmd_rx: mpsc::Receiver<Command>) {
  let mut ring = match io_uring::IoUring::new(entries) {
    Ok(r) => r,
    Err(_) => {
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }
  };

  let ring_fd = ring.as_raw_fd();

  let mut ops: HashMap<u64, Arc<IoOp>> = HashMap::new();

  loop {
    while let Ok(cmd) = cmd_rx.try_recv() {
      match cmd {
        Command::SubmitRead { op, fd } => {
          if op.cancel_requested() {
            op.complete(Err(IoError::Cancelled));
            continue;
          }
          let ud = user_data(op.id, UserDataKind::Read);
          let sqe = opcode::Read::new(types::Fd(fd), op.ptr, op.len as _)
            .build()
            .user_data(ud);
          unsafe {
            ring.submission().push(&sqe).ok();
          }
          ops.insert(decode_user_data_id(ud), op);
        }
        Command::Cancel { id } => {
          let target = user_data(id, UserDataKind::Read);
          let ud = user_data(id, UserDataKind::Cancel);
          let sqe = opcode::AsyncCancel::new(target).build().user_data(ud);
          unsafe {
            ring.submission().push(&sqe).ok();
          }
        }
        Command::Shutdown => {
          unsafe {
            libc::close(wake_fd);
          }
          return;
        }
      }
    }

    let _ = ring.submit();

    let mut fds = [
      libc::pollfd {
        fd: ring_fd,
        events: libc::POLLIN,
        revents: 0,
      },
      libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
      },
    ];
    unsafe {
      libc::poll(fds.as_mut_ptr(), fds.len() as _, -1);
    }

    if fds[1].revents & libc::POLLIN != 0 {
      let mut buf = [0u8; 8];
      unsafe {
        libc::read(wake_fd, buf.as_mut_ptr() as *mut libc::c_void, 8);
      }
    }

    let mut cq = ring.completion();
    for cqe in &mut cq {
      let ud = cqe.user_data();
      let id = decode_user_data_id(ud);
      match decode_user_data_kind(ud) {
        UserDataKind::Read => {
          if let Some(op) = ops.remove(&id) {
            let res = cqe.result();
            if res >= 0 {
              op.complete(Ok(res as usize));
            } else if res == -libc::ECANCELED {
              op.complete(Err(IoError::Cancelled));
            } else {
              op.complete(Err(IoError::Uring(res)));
            }
          }
        }
        UserDataKind::Cancel => {}
      }
    }
  }
}
