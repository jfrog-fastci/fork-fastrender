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
    {
      let ring = io_uring::IoUring::new(entries)?;
      thread::spawn(move || run_driver(ring, wake_fd, cmd_rx));
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
      std::io::Error::new(std::io::ErrorKind::Other, "io_uring submission queue is full")
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
    if rc >= 0 {
      continue;
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
fn run_driver(mut ring: io_uring::IoUring, wake_fd: RawFd, cmd_rx: mpsc::Receiver<Command>) {
  fn submit_errno(err: &std::io::Error) -> i32 {
    -(err.raw_os_error().unwrap_or(libc::EIO))
  }

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
          if let Err(err) = push_sqe_with_backpressure(&mut ring, &sqe) {
            op.complete(Err(IoError::Uring(submit_errno(&err))));
            continue;
          }
          ops.insert(decode_user_data_id(ud), op);
        }
        Command::Cancel { id } => {
          let target = user_data(id, UserDataKind::Read);
          let ud = user_data(id, UserDataKind::Cancel);
          let sqe = opcode::AsyncCancel::new(target).build().user_data(ud);
          let _ = push_sqe_with_backpressure(&mut ring, &sqe);
        }
        Command::Shutdown => {
          for (_, op) in ops.drain() {
            op.complete(Err(IoError::Cancelled));
          }
          unsafe {
            libc::close(wake_fd);
          }
          return;
        }
      }
    }

    if let Err(err) = submit_with_retry(&mut ring) {
      let code = submit_errno(&err);
      for (_, op) in ops.drain() {
        op.complete(Err(IoError::Uring(code)));
      }
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
    loop {
      let poll_rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
      if poll_rc >= 0 {
        break;
      }
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      let code = submit_errno(&err);
      for (_, op) in ops.drain() {
        op.complete(Err(IoError::Uring(code)));
      }
      unsafe {
        libc::close(wake_fd);
      }
      return;
    }

    if fds[1].revents & libc::POLLIN != 0 {
      drain_eventfd(wake_fd);
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
#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use crate::gc::TypeDescriptor;
  use crate::test_util::TestRuntimeGuard;
  use crate::ArrayBuffer;
  use std::os::fd::{FromRawFd, OwnedFd};
  use std::task::Wake;
  use std::time::{Duration, Instant};

  static ARRAY_BUFFER_DESC: TypeDescriptor = TypeDescriptor::new(
    OBJ_HEADER_SIZE + core::mem::size_of::<ArrayBuffer>(),
    &[],
  );
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
    let buffer_payload = unsafe { &*(buffer.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    let view = Uint8Array::view(buffer_payload, byte_offset, length).unwrap();
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

  #[test]
  fn batches_reads_without_losing_ops_when_sq_is_full() {
    let _rt = TestRuntimeGuard::new();

    let heap = Arc::new(Mutex::new(GcHeap::new()));
    let driver = UringDriver::new(2).unwrap();

    let mut read_futs: Vec<(ReadFuture, *const u8, u8, *mut u8)> = Vec::new();
    let mut read_fds: Vec<OwnedFd> = Vec::new();
    let mut write_fds: Vec<OwnedFd> = Vec::new();

    for idx in 0..4u8 {
      let (rfd, wfd) = pipe().unwrap();

      let (array_obj, buffer_obj, promise_obj, bytes_ptr) = {
        let mut heap = heap.lock().unwrap();
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
      ));
      assert!(pin_count(buffer_obj) > 0);

      driver
        .inner
        .cmd_tx
        .send(Command::SubmitRead {
          op: Arc::clone(&op),
          fd: rfd.as_raw_fd(),
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
    driver.signal();

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
