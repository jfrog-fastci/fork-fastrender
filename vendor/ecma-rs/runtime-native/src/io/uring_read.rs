#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

#[cfg(target_os = "linux")]
use io_uring::opcode;
#[cfg(target_os = "linux")]
use io_uring::types;

use crate::buffer::typed_array::{PinnedUint8Array, TypedArrayError, Uint8Array};
use crate::buffer::BorrowGuardWrite;
use crate::gc::{GcHeap, RootHandle, OBJ_HEADER_SIZE};
use crate::threading;
use crate::threading::ThreadKind;

use super::limits::{IoLimitError, IoLimits, IoLimiter, IoPermit};

#[derive(Debug, thiserror::Error)]
pub enum IoError {
  #[error("operation cancelled")]
  Cancelled,
  #[error("I/O limits exceeded: {0}")]
  Limits(IoLimitError),
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
  permit: Mutex<Option<IoPermit>>,
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
    permit: IoPermit,
  ) -> Self {
    let (buffer_root, promise_root) = {
      let mut heap_lock = heap.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      (
        heap_lock.root_add(buffer_obj),
        heap_lock.root_add(promise_obj),
      )
    };

    let ptr = buf.as_ptr();
    let len = buf.len();

    Self {
      id,
      buf: Mutex::new(Some(buf)),
      borrow: Mutex::new(Some(borrow)),
      permit: Mutex::new(Some(permit)),
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
      let mut heap_lock = self
        .heap
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    self
      .permit
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take();

    {
      let mut lock = self
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let mut lock = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
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
      let mut lock = this
        .op
        .waker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

impl Drop for ReadFuture {
  fn drop(&mut self) {
    // If the operation already completed, there is nothing to cancel.
    if self.op.completion_started.load(Ordering::Acquire) {
      return;
    }

    // Dropping the future means no one is waiting on the result anymore. Request an io_uring cancel
    // so pinned backing stores and I/O limiter permits are released promptly.
    if self.op.mark_cancel_requested() {
      self.driver.request_cancel(self.op.id);
    }
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
  SubmitRead {
    op: Arc<IoOp>,
    fd: RawFd,
    in_flight: InFlightToken,
  },
  Cancel { id: u64 },
  #[cfg(test)]
  Barrier(mpsc::Sender<()>),
  Shutdown,
  ShutdownAndDrain {
    done: mpsc::Sender<std::io::Result<()>>,
  },
}

#[derive(Debug)]
struct DriverShared {
  in_flight_ops: AtomicUsize,
  closing: AtomicBool,
  leak_on_drop: AtomicBool,
}

#[derive(Debug)]
struct InFlightToken {
  shared: Arc<DriverShared>,
}

impl InFlightToken {
  fn new(shared: Arc<DriverShared>) -> Self {
    shared.in_flight_ops.fetch_add(1, Ordering::Relaxed);
    Self { shared }
  }
}

impl Drop for InFlightToken {
  fn drop(&mut self) {
    let prev = self.shared.in_flight_ops.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "in_flight_ops underflow");
  }
}

#[derive(Debug)]
struct DriverInner {
  cmd_tx: mpsc::Sender<Command>,
  wake_fd: RawFd,
  next_id: AtomicU64,
  limiter: Arc<IoLimiter>,
  shared: Arc<DriverShared>,
}

#[cfg(target_os = "linux")]
fn wake_eventfd(fd: RawFd) {
  let val: u64 = 1;
  loop {
    let rc = unsafe {
      libc::write(
        fd,
        (&val as *const u64).cast::<libc::c_void>(),
        core::mem::size_of::<u64>(),
      )
    };

    if rc == core::mem::size_of::<u64>() as isize {
      return;
    }
    if rc == -1 {
      let err = std::io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EINTR) => continue,
        // Counter overflow is practically impossible; treat EAGAIN as coalescing.
        Some(libc::EAGAIN) => return,
        _ => return,
      }
    }
    // Unexpected short write: treat it as a best-effort wake-up and avoid looping forever.
    return;
  }
}

impl Drop for DriverInner {
  fn drop(&mut self) {
    // Stop accepting new work immediately.
    self.shared.closing.store(true, Ordering::Release);

    let in_flight = self.shared.in_flight_ops.load(Ordering::Acquire);
    if in_flight != 0 {
      // Dropping the driver with in-flight ops is not allowed. Tell the driver thread to *leak*
      // any remaining state (ring + ops) rather than attempting CQE-driven completion.
      self.shared.leak_on_drop.store(true, Ordering::Release);
    }

    // Ask the driver thread to stop. If ops are still in-flight, the driver thread will *leak* the
    // ring and in-flight state (see `run_driver`), keeping any SQE-referenced pointers valid until
    // the kernel is done.
    //
    // Best-effort: ignore send errors (driver thread may have already exited).
    if self.cmd_tx.send(Command::Shutdown).is_ok() {
      wake_eventfd(self.wake_fd);
    }

    // Policy B (mirrors `runtime-io-uring`):
    // - In debug builds, dropping with in-flight ops is a bug: leak first, then panic (unless
    //   already unwinding).
    // - In release builds, leak to preserve memory safety.
    if in_flight != 0 && cfg!(debug_assertions) && !std::thread::panicking() {
      // Note: we *must* wake/leak before panicking. Panicking in `Drop` does not guarantee fields
      // won't be dropped during unwinding.
      panic!(
        "runtime-native: dropping UringDriver with {in_flight} in-flight ops; \
         call UringDriver::shutdown_and_drain() before drop"
      );
    }
  }
}

/// Minimal io_uring driver used by the runtime-native I/O bridge layer.
///
/// This is intentionally narrow in scope: it exists to safely submit kernel operations that write
/// directly into a GC-managed `Uint8Array` backing store, while keeping the JS values rooted until
/// completion.
///
/// # Teardown / drop semantics (io_uring lifetime policy B)
///
/// io_uring SQEs may contain raw user pointers (e.g. read buffers). Those pointers must remain
/// valid until the kernel produces the corresponding CQE.
///
/// To preserve memory safety, this driver follows "policy B" (see `docs/io_uring_lifetimes.md`):
///
/// - Callers must use [`UringDriver::shutdown_and_drain`] to shut down the driver and wait until
///   all in-flight operations have reached a terminal CQE before dropping pinned buffers/roots.
/// - Dropping a driver with in-flight operations is considered a bug:
///   - In debug builds, it **panics** (unless already unwinding) after requesting a leak-safe
///     shutdown.
///   - In release builds, it **leaks** the ring + in-flight state (safe-by-leak).
#[derive(Clone, Debug)]
pub struct UringDriver {
  inner: Arc<DriverInner>,
}

impl UringDriver {
  pub fn new(entries: u32) -> Result<Self, std::io::Error> {
    Self::new_with_limits(entries, IoLimits::default())
  }

  pub fn new_with_limits(entries: u32, limits: IoLimits) -> Result<Self, std::io::Error> {
    Self::new_with_limiter(entries, Arc::new(IoLimiter::new(limits)))
  }

  pub fn new_with_limiter(entries: u32, limiter: Arc<IoLimiter>) -> Result<Self, std::io::Error> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let wake_fd = loop {
      let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
      if wake_fd >= 0 {
        break wake_fd;
      }
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      return Err(err);
    };

    let shared = Arc::new(DriverShared {
      in_flight_ops: AtomicUsize::new(0),
      closing: AtomicBool::new(false),
      leak_on_drop: AtomicBool::new(false),
    });

    #[cfg(target_os = "linux")]
    {
      let ring = match io_uring::IoUring::new(entries) {
        Ok(ring) => ring,
        Err(err) => {
          unsafe {
            libc::close(wake_fd);
          }
          return Err(err);
        }
      };
      let shared = Arc::clone(&shared);
      thread::spawn(move || run_driver(ring, wake_fd, cmd_rx, shared));
    }
    #[cfg(not(target_os = "linux"))]
    {
      let _ = entries;
      let _ = cmd_rx;
      unsafe {
        libc::close(wake_fd);
      }
      return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "io_uring is only supported on linux",
      ));
    }

    Ok(Self {
      inner: Arc::new(DriverInner {
        cmd_tx,
        wake_fd,
        next_id: AtomicU64::new(1),
        limiter,
        shared,
      }),
    })
  }

  fn next_op_id(&self) -> u64 {
    self.inner.next_id.fetch_add(1, Ordering::Relaxed)
  }

  fn request_cancel(&self, id: u64) {
    if self.inner.cmd_tx.send(Command::Cancel { id }).is_ok() {
      self.signal();
    }
  }

  fn signal(&self) {
    wake_eventfd(self.inner.wake_fd);
  }

  /// Shut down the driver thread and synchronously drain all in-flight operations.
  ///
  /// This is the **only** safe way to tear down a driver without leaking resources. It ensures
  /// all SQE-referenced user pointers remain valid until the kernel produces the final CQE for
  /// each operation.
  pub fn shutdown_and_drain(&self) -> std::io::Result<()> {
    // Prevent new submissions from being accepted.
    self.inner.shared.closing.store(true, Ordering::Release);

    let (done_tx, done_rx) = mpsc::channel::<std::io::Result<()>>();
    // Best-effort: if the driver thread is already gone, consider it drained.
    if self
      .inner
      .cmd_tx
      .send(Command::ShutdownAndDrain { done: done_tx })
      .is_err()
    {
      return Ok(());
    }
    self.signal();
    done_rx.recv().unwrap_or_else(|_| {
      Err(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "io_uring driver thread terminated before draining",
      ))
    })
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
    if self.inner.shared.closing.load(Ordering::Acquire) {
      return Err(IoError::Cancelled);
    }
    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
      return Err(IoError::Cancelled);
    }

    // Safety: callers promise `array_obj` points to a `Uint8Array` object payload after ObjHeader.
    let view = unsafe { &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array) };
    let store = view
      .backing_store_handle()
      .map_err(IoError::InvalidBuffer)?;
    let pinned_bytes = store.alloc_len();
    let permit = self
      .inner
      .limiter
      .try_acquire(pinned_bytes)
      .map_err(IoError::Limits)?;

    let pinned = view.pin().map_err(IoError::InvalidBuffer)?;
    let borrow = pinned
      .backing_store()
      .try_borrow_io_write()
      .map_err(|_| IoError::BufferBorrowed)?;

    let id = self.next_op_id();
    let op = Arc::new(IoOp::new(
      id,
      heap,
      array_obj,
      promise_obj,
      pinned,
      borrow,
      permit,
    ));

    if self
      .inner
      .cmd_tx
      .send(Command::SubmitRead {
        op: Arc::clone(&op),
        fd,
        in_flight: InFlightToken::new(Arc::clone(&self.inner.shared)),
      })
      .is_err()
    {
      op.complete(Err(IoError::Uring(-libc::EPIPE)));
      return Err(IoError::Uring(-libc::EPIPE));
    }
    self.signal();

    Ok(ReadFuture {
      op,
      driver: self.clone(),
      cancel,
    })
  }
}

#[cfg(target_os = "linux")]
fn submit_with_retry(ring: &mut io_uring::IoUring) -> std::io::Result<usize> {
  loop {
    match ring.submit() {
      Ok(n) => return Ok(n),
      Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
      Err(err) => return Err(err),
    }
  }
}

#[cfg(target_os = "linux")]
fn push_sqe_with_backpressure(
  ring: &mut io_uring::IoUring,
  sqe: &io_uring::squeue::Entry,
) -> std::io::Result<()> {
  // SAFETY: `sqe` is copied into the submission ring by `io_uring`.
  unsafe {
    if ring.submission().push(sqe).is_ok() {
      return Ok(());
    }
  }

  // SQ is full; flush the current batch to the kernel to make room.
  submit_with_retry(ring)?;

  // SAFETY: `sqe` is copied into the submission ring by `io_uring`.
  unsafe {
    ring.submission().push(sqe).map_err(|_| {
      std::io::Error::new(
        std::io::ErrorKind::Other,
        "io_uring submission queue is full",
      )
    })?;
  }

  Ok(())
}

#[cfg(target_os = "linux")]
fn drain_eventfd(fd: RawFd) {
  let mut buf = [0u8; 8];
  loop {
    // SAFETY: `buf` is a valid writable buffer and `fd` is expected to be an eventfd.
    let rc = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if rc == buf.len() as isize {
      continue;
    }
    if rc >= 0 {
      // Unexpected EOF or short read; treat as drained.
      return;
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
      Some(libc::EINTR) => continue,
      Some(libc::EAGAIN) => return,
      _ => return,
    }
  }
}

#[cfg(target_os = "linux")]
fn run_driver(
  mut ring: io_uring::IoUring,
  wake_fd: RawFd,
  cmd_rx: mpsc::Receiver<Command>,
  shared: Arc<DriverShared>,
) {
  threading::register_current_thread(ThreadKind::Io);
  struct Unregister;
  impl Drop for Unregister {
    fn drop(&mut self) {
      threading::unregister_current_thread();
    }
  }
  let _unregister = Unregister;

  fn submit_errno(err: &std::io::Error) -> i32 {
    -(err.raw_os_error().unwrap_or(libc::EIO))
  }

  let ring_fd = ring.as_raw_fd();

  let mut ops: HashMap<u64, (Arc<IoOp>, InFlightToken)> = HashMap::new();
  let mut pending_cancels: VecDeque<u64> = VecDeque::new();
  let mut cancel_in_flight: usize = 0;

  let mut shutdown = false;
  let mut drain_done: Option<mpsc::Sender<std::io::Result<()>>> = None;
  let mut drain_cancels_enqueued = false;

  loop {
    while let Ok(cmd) = cmd_rx.try_recv() {
      match cmd {
        Command::SubmitRead { op, fd, in_flight } => {
          // If the driver is being dropped with in-flight ops (policy B), do not attempt to submit
          // or complete anything. Retain the op state and leak it on shutdown so pinned buffers and
          // GC roots remain valid.
          if shared.leak_on_drop.load(Ordering::Acquire) {
            ops.insert(op.id, (op, in_flight));
            continue;
          }

          // Once the driver begins shutting down, do not submit any more SQEs. These ops were
          // never handed to the kernel, so it's safe to complete and drop their resources here.
          if shared.closing.load(Ordering::Acquire) {
            op.complete(Err(IoError::Cancelled));
            continue;
          }

          if op.cancel_requested() {
            op.complete(Err(IoError::Cancelled));
            continue;
          }
          let ud = user_data(op.id, UserDataKind::Read);
          let sqe = opcode::Read::new(types::Fd(fd), op.ptr, op.len as _)
            .build()
            .user_data(ud);
          if let Err(err) = push_sqe_with_backpressure(&mut ring, &sqe) {
            op.complete(Err(IoError::Uring(submit_errno(&err))));
            continue;
          }
          ops.insert(decode_user_data_id(ud), (op, in_flight));
        }
        Command::Cancel { id } => {
          pending_cancels.push_back(id);
        }
        #[cfg(test)]
        Command::Barrier(done) => {
          // Ensure any previously enqueued SQEs are flushed to the kernel before signaling.
          let _ = submit_with_retry(&mut ring);
          let _ = done.send(());
        }
        Command::Shutdown => {
          shared.closing.store(true, Ordering::Release);

          // Policy B shutdown: never drop SQE-referenced pointers without CQE-driven completion.
          //
          // If we still have in-flight ops, leak the ring and op state to preserve memory safety.
          // If no ops are in-flight, exit cleanly.
          let should_leak = shared.leak_on_drop.load(Ordering::Acquire)
            || shared.in_flight_ops.load(Ordering::Acquire) != 0
            || !ops.is_empty();

          if should_leak {
            // Leak before exiting: dropping `IoUring` would unmap the shared rings while the kernel
            // may still write CQEs, and dropping `ops` would release pinned buffers.
            //
            // Note: we do *not* attempt CQE-driven completion here, because the driver is being
            // dropped without an explicit drain request.
            std::mem::forget(ops);
            std::mem::forget(ring);
            if let Some(done) = drain_done.take() {
              let _ = done.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io_uring driver thread terminated during shutdown_and_drain",
              )));
            }
            unsafe {
              libc::close(wake_fd);
            }
            return;
          }

          // No read ops in flight. We still must not drop `ring` while any cancel SQEs are in
          // flight: the kernel may still write cancel CQEs into the ring's shared memory mappings.
          // Drive the loop until `cancel_in_flight == 0`, then exit cleanly.
          shutdown = true;
        }
        Command::ShutdownAndDrain { done } => {
          // Enter drain mode:
          // - stop accepting new submissions,
          // - best-effort cancel in-flight reads,
          // - keep polling CQEs until all reads complete, then exit.
          shared.closing.store(true, Ordering::Release);
          // Only the first drain request wins (subsequent calls will observe a broken pipe).
          if drain_done.is_none() {
            drain_done = Some(done);
          }
        }
      }
    }

    // Drain request: cancel any still-in-flight operations so pinned backing stores and limiter
    // permits are released before we exit the thread.
    if drain_done.is_some() && !drain_cancels_enqueued {
      pending_cancels.extend(ops.keys().copied());
      drain_cancels_enqueued = true;
    }

    // If the driver is being dropped with in-flight ops, leak the ring + op state immediately and
    // stop processing CQEs. This preserves SQE pointer lifetimes without blocking.
    if shared.leak_on_drop.load(Ordering::Acquire) {
      // Drain any queued commands so their op state is retained in `ops` before we leak it.
      while let Ok(cmd) = cmd_rx.try_recv() {
        if let Command::SubmitRead { op, in_flight, .. } = cmd {
          ops.insert(op.id, (op, in_flight));
        } else if let Command::ShutdownAndDrain { done } = cmd {
          let _ = done.send(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "io_uring driver thread was dropped during shutdown_and_drain",
          )));
        }
      }

      if let Some(done) = drain_done.take() {
        let _ = done.send(Err(std::io::Error::new(
          std::io::ErrorKind::BrokenPipe,
          "io_uring driver thread was dropped during shutdown_and_drain",
        )));
      }

      std::mem::forget(ops);
      std::mem::forget(ring);
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    // Drain any queued cancellation requests. If the SQ is temporarily full, keep the id queued and
    // retry on the next loop turn.
    while let Some(id) = pending_cancels.front().copied() {
      if !ops.contains_key(&id) {
        // Either the op hasn't been submitted yet (in which case `cancel_requested` will prevent it
        // from being queued) or it already completed. No kernel cancel needed.
        pending_cancels.pop_front();
        continue;
      }

      let target = user_data(id, UserDataKind::Read);
      let ud = user_data(id, UserDataKind::Cancel);
      let sqe = opcode::AsyncCancel::new(target).build().user_data(ud);
      match push_sqe_with_backpressure(&mut ring, &sqe) {
        Ok(()) => {
          pending_cancels.pop_front();
          cancel_in_flight = cancel_in_flight.saturating_add(1);
        }
        Err(_) => break,
      }
    }

    // Drain request with no in-flight reads: exit without blocking in `poll`.
    if drain_done.is_some() && ops.is_empty() && cancel_in_flight == 0 && pending_cancels.is_empty() {
      let done = drain_done.take().expect("checked above");
      let _ = done.send(Ok(()));
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    // Drop request with no in-flight reads/cancels: exit without blocking in `poll`.
    if shutdown && ops.is_empty() && cancel_in_flight == 0 && pending_cancels.is_empty() {
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    if let Err(err) = submit_with_retry(&mut ring) {
      // Fatal: we can no longer reliably drive CQE-based completion.
      // Leak any in-flight ops to preserve pointer lifetimes.
      if let Some(done) = drain_done.take() {
        let _ = done.send(Err(err));
      }
      std::mem::forget(ops);
      std::mem::forget(ring);
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

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
    let timeout_ms = if pending_cancels.is_empty() { -1 } else { 10 };
    loop {
      let poll_rc =
        threading::park_while(|| unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) });
      if poll_rc >= 0 {
        break;
      }
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      // Fatal poll error: leak ring and ops to preserve memory safety.
      if let Some(done) = drain_done.take() {
        let _ = done.send(Err(err));
      }
      std::mem::forget(ops);
      std::mem::forget(ring);
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    if fds[1].revents & libc::POLLIN != 0 {
      drain_eventfd(wake_fd);
    }

    // If a drop request raced with `poll`, leak immediately and stop processing CQEs to avoid
    // releasing SQE-referenced pointers.
    if shared.leak_on_drop.load(Ordering::Acquire) {
      while let Ok(cmd) = cmd_rx.try_recv() {
        if let Command::SubmitRead { op, in_flight, .. } = cmd {
          ops.insert(op.id, (op, in_flight));
        }
      }
      std::mem::forget(ops);
      std::mem::forget(ring);
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    let mut cq = ring.completion();
    for cqe in &mut cq {
      let ud = cqe.user_data();
      let id = decode_user_data_id(ud);
      match decode_user_data_kind(ud) {
        UserDataKind::Read => {
          if let Some((op, _in_flight)) = ops.remove(&id) {
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
        UserDataKind::Cancel => {
          cancel_in_flight = cancel_in_flight.saturating_sub(1);
        }
      }
    }

    // If we're draining and there are no more in-flight reads, exit cleanly and notify the waiter.
    if drain_done.is_some() && ops.is_empty() && cancel_in_flight == 0 && pending_cancels.is_empty() {
      let done = drain_done.take().expect("checked above");
      let _ = done.send(Ok(()));
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    // If the driver was dropped and there are no more in-flight reads/cancels, exit cleanly.
    if shutdown && ops.is_empty() && cancel_in_flight == 0 && pending_cancels.is_empty() {
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }
  }
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use crate::gc::TypeDescriptor;
  use crate::test_util::TestRuntimeGuard;
  use crate::ArrayBuffer;
  use std::io;
  use std::os::fd::{FromRawFd, OwnedFd};
  use std::task::Wake;
  use std::time::{Duration, Instant};

  static ARRAY_BUFFER_DESC: TypeDescriptor =
    TypeDescriptor::new(OBJ_HEADER_SIZE + core::mem::size_of::<ArrayBuffer>(), &[]);
  static UINT8_ARRAY_PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
  static UINT8_ARRAY_DESC: TypeDescriptor = TypeDescriptor::new(
    OBJ_HEADER_SIZE + core::mem::size_of::<Uint8Array>(),
    &UINT8_ARRAY_PTR_OFFSETS,
  );
  static DUMMY_DESC: TypeDescriptor = TypeDescriptor::new(OBJ_HEADER_SIZE, &[]);

  fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
      return Err(std::io::Error::last_os_error());
    }
    // Safety: `pipe` returns new, owned file descriptors.
    let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((rfd, wfd))
  }

  fn write_all(fd: RawFd, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
      let rc = unsafe {
        libc::write(
          fd,
          bytes[written..].as_ptr() as *const libc::c_void,
          bytes.len() - written,
        )
      };
      assert!(rc >= 0, "write failed: {}", std::io::Error::last_os_error());
      written += rc as usize;
    }
  }

  struct FlagWake {
    flag: Arc<AtomicBool>,
  }

  impl Wake for FlagWake {
    fn wake(self: Arc<Self>) {
      self.flag.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
      self.flag.store(true, Ordering::SeqCst);
    }
  }

  fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
    Waker::from(Arc::new(FlagWake { flag }))
  }

  fn block_on<F: Future>(fut: F, timeout: Duration) -> F::Output {
    let woke = Arc::new(AtomicBool::new(false));
    let waker = flag_waker(woke.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    let deadline = Instant::now() + timeout;

    loop {
      match fut.as_mut().poll(&mut cx) {
        Poll::Ready(out) => return out,
        Poll::Pending => {
          while !woke.swap(false, Ordering::SeqCst) {
            if Instant::now() > deadline {
              panic!("timed out waiting for future");
            }
            std::thread::yield_now();
          }
        }
      }
    }
  }

  fn alloc_array_buffer(heap: &mut GcHeap, byte_len: usize) -> *mut u8 {
    // Allocate the ArrayBuffer header in old gen so it does not move during nursery evacuation. The
    // backing store itself is always non-moving.
    let obj = heap.alloc_old(&ARRAY_BUFFER_DESC);
    let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer };
    let header = ArrayBuffer::new_zeroed(byte_len).unwrap();
    unsafe {
      payload.write(header);
    }
    obj
  }

  fn alloc_uint8_array(
    heap: &mut GcHeap,
    buffer: *mut u8,
    byte_offset: usize,
    length: usize,
  ) -> *mut u8 {
    let obj = heap.alloc_young(&UINT8_ARRAY_DESC);
    let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut Uint8Array };
    let view = Uint8Array::view_gc(buffer, byte_offset, length).unwrap();
    unsafe {
      payload.write(view);
    }
    obj
  }

  fn alloc_dummy(heap: &mut GcHeap) -> *mut u8 {
    heap.alloc_young(&DUMMY_DESC)
  }

  fn finalize_array_buffer(buffer_obj: *mut u8) {
    // Safety: payload layout matches `ArrayBuffer`.
    let buf = unsafe { &mut *(buffer_obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer) };
    buf.finalize();
  }

  fn pin_count(buffer_obj: *mut u8) -> u32 {
    let buf = unsafe { &*(buffer_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    buf.pin_count()
  }

  fn is_uring_unavailable(err: &io::Error) -> bool {
    matches!(
      err.raw_os_error(),
      Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
  }

  #[test]
  fn batches_reads_without_losing_ops_when_sq_is_full() {
    let _rt = TestRuntimeGuard::new();

    let heap = Arc::new(Mutex::new(GcHeap::new()));
    let driver = match UringDriver::new(2) {
      Ok(driver) => driver,
      Err(err) if is_uring_unavailable(&err) => return,
      Err(err) => panic!("failed to create io_uring driver: {err:?}"),
    };

    let mut read_futs: Vec<(ReadFuture, *const u8, u8, *mut u8)> = Vec::new();
    let mut read_fds: Vec<OwnedFd> = Vec::new();
    let mut write_fds: Vec<OwnedFd> = Vec::new();

    for idx in 0..4u8 {
      let (rfd, wfd) = pipe().unwrap();

      let (array_obj, buffer_obj, promise_obj, bytes_ptr) = {
        let mut heap = heap.lock().unwrap_or_else(|e| e.into_inner());
        let buffer_obj = alloc_array_buffer(&mut heap, 1);
        let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 1);
        let promise_obj = alloc_dummy(&mut heap);

        let bytes_ptr = unsafe {
          let view = &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array);
          view.as_ptr_range().unwrap().0 as *const u8
        };

        (array_obj, buffer_obj, promise_obj, bytes_ptr)
      };

      // Mirror `UringDriver::read_into_uint8_array`, but enqueue a batch of commands and only wake
      // the driver once to deterministically exercise SQ backpressure.
      let pinned = unsafe { &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array) }
        .pin()
        .expect("pin should succeed");
      let permit = driver
        .inner
        .limiter
        .try_acquire(pinned.backing_store_alloc_len())
        .expect("io permit should be available");
      let borrow = pinned
        .backing_store()
        .try_borrow_io_write()
        .expect("buffer should not already be borrowed");
      let id = driver.next_op_id();
      let op = Arc::new(IoOp::new(
        id,
        Arc::clone(&heap),
        array_obj,
        promise_obj,
        pinned,
        borrow,
        permit,
      ));
      assert!(pin_count(buffer_obj) > 0);

      driver
        .inner
        .cmd_tx
        .send(Command::SubmitRead {
          op: Arc::clone(&op),
          fd: rfd.as_raw_fd(),
          in_flight: InFlightToken::new(Arc::clone(&driver.inner.shared)),
        })
        .unwrap();

      read_futs.push((
        ReadFuture {
          op,
          driver: driver.clone(),
          cancel: None,
        },
        bytes_ptr,
        idx,
        buffer_obj,
      ));
      read_fds.push(rfd);
      write_fds.push(wfd);
    }

    // Wake the driver once so it drains the whole batch in one go (ensuring it sees SQ full).
    // Use a barrier so we don't start writing until all SQEs are flushed to the kernel.
    let (tx, rx) = mpsc::channel();
    driver.inner.cmd_tx.send(Command::Barrier(tx)).unwrap();
    driver.signal();
    rx.recv_timeout(Duration::from_secs(1)).unwrap();

    for (idx, wfd) in write_fds.iter().enumerate() {
      let byte = idx as u8;
      write_all(wfd.as_raw_fd(), &[byte]);
    }

    for (fut, bytes_ptr, expected, buffer_obj) in read_futs {
      let n = block_on(fut, Duration::from_secs(2)).unwrap();
      assert_eq!(n, 1);
      let got = unsafe { std::slice::from_raw_parts(bytes_ptr, n) };
      assert_eq!(got, &[expected]);
      assert_eq!(pin_count(buffer_obj), 0);
      finalize_array_buffer(buffer_obj);
    }
  }
}
