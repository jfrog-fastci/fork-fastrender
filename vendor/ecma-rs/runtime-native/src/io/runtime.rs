use crate::abi::PromiseRef;
use crate::buffer::Uint8Array;
use crate::sync::GcAwareMutex;
use crate::threading::ThreadKind;
use crate::{async_rt, threading};
use std::io;
use std::ops::Range;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use super::limits::{IoCounters, IoLimiter, IoLimits};
use super::op_registry::{IoOpDebugHooks, IoOpId, IoOpKind, IoOpOutcome, IoOpRecord, OpRegistry, RootPin};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeState {
  Running = 0,
  Detached = 1,
}

impl RuntimeState {
  fn from_u8(v: u8) -> Self {
    match v {
      0 => RuntimeState::Running,
      _ => RuntimeState::Detached,
    }
  }
}

struct IoRuntimeInner {
  state: AtomicU8,
  registry: GcAwareMutex<OpRegistry>,
  limiter: Arc<IoLimiter>,
}

impl IoRuntimeInner {
  fn state(&self) -> RuntimeState {
    RuntimeState::from_u8(self.state.load(Ordering::Acquire))
  }

  fn set_detached(&self) {
    self.state.store(RuntimeState::Detached as u8, Ordering::Release);
  }

  fn enqueue_completion(this: Weak<IoRuntimeInner>, id: IoOpId) {
    struct Completion {
      rt: Weak<IoRuntimeInner>,
      id: IoOpId,
    }

    extern "C" fn run(data: *mut u8) {
      // Safety: allocated via `Box::into_raw` in `enqueue_completion`, and freed by the task drop
      // hook.
      let st = unsafe { &*(data as *const Completion) };

      let Some(rt) = st.rt.upgrade() else {
        return;
      };

      // Don't execute JS-facing completion logic once the runtime is detached (realm/VM teardown or
      // hard termination).
      if rt.state() != RuntimeState::Running {
        // Drop outside the registry lock: dropping an op record can unregister GC root pins.
        let removed = rt.registry.lock().remove(st.id);
        drop(removed);
        return;
      }

      let Some(op) = rt.registry.lock().remove(st.id) else {
        return;
      };

      match op.take_outcome().unwrap_or(IoOpOutcome::Canceled) {
        IoOpOutcome::Ok(_) => {
          async_rt::promise::promise_resolve(op.promise, core::ptr::null_mut());
        }
        IoOpOutcome::Err(_) | IoOpOutcome::Canceled => {
          async_rt::promise::promise_reject(op.promise, core::ptr::null_mut());
        }
      }
      // Dropping `op` releases the pinned buffers/permit and any GC roots.
    }

    extern "C" fn drop_completion(data: *mut u8) {
      // Safety: allocated by `Box::into_raw` below.
      unsafe {
        drop(Box::from_raw(data as *mut Completion));
      }
    }

    // Use a drop hook so the boxed completion state is freed even if the event loop is torn down
    // before the task is executed.
    async_rt::global().enqueue_macrotask(async_rt::Task::new_with_drop(
      run,
      Box::into_raw(Box::new(Completion { rt: this, id })) as *mut u8,
      drop_completion,
    ));
  }

  fn teardown(&self) {
    self.set_detached();

    // Drain all active ops from the registry. Don't drop their pinned buffers/roots until the
    // worker thread has observed cancellation and returned (it holds a strong `Arc<IoOpRecord>`).
    let ops = self.registry.lock().drain();
    for op in ops {
      op.cancel.cancel();
    }
  }
}

/// Per-realm/per-isolate async I/O runtime.
///
/// This owns a registry of active operations. When the realm/VM is shutting down (or a hard
/// termination occurs), call [`IoRuntime::teardown`] to:
/// - request cancellation of all in-flight ops, and
/// - detach completion callbacks so no JS heap access occurs.
pub struct IoRuntime {
  inner: Arc<IoRuntimeInner>,
}

impl IoRuntime {
  pub fn new() -> Self {
    Self::new_with_limits(IoLimits::default())
  }

  pub fn new_with_limits(limits: IoLimits) -> Self {
    // Ensure the process-global runtime infrastructure is initialized.
    let _ = crate::rt_ensure_init();
    let _ = async_rt::global();
    threading::register_current_thread(ThreadKind::Main);

    Self {
      inner: Arc::new(IoRuntimeInner {
        state: AtomicU8::new(RuntimeState::Running as u8),
        registry: GcAwareMutex::new(OpRegistry::new()),
        limiter: Arc::new(IoLimiter::new(limits)),
      }),
    }
  }

  pub fn teardown(&self) {
    self.inner.teardown();
  }

  pub fn cancel_all(&self) {
    self.teardown();
  }

  /// Submit a non-blocking `write(2)` operation and return the associated promise.
  ///
  /// The write is performed on a dedicated OS thread. The caller provides a stable backing store
  /// (`Uint8Array` backed by a non-moving `BackingStore`), and the bytes are pinned + accounted for
  /// via [`super::op::IoOp`].
  pub fn write(&self, fd: OwnedFd, view: &Uint8Array, range: Range<usize>) -> io::Result<PromiseRef> {
    self.write_with_debug_hooks(fd, view, range, &[], None)
  }

  /// Submit a non-blocking `read(2)` operation and return the associated promise.
  ///
  /// The read is performed on a dedicated OS thread. The bytes are pinned (pointer stability) and
  /// the backing store is exclusively borrowed for the lifetime of the op (data-race safety while
  /// the kernel writes into the buffer).
  pub fn read(&self, fd: OwnedFd, view: &Uint8Array, range: Range<usize>) -> io::Result<PromiseRef> {
    self.read_with_debug_hooks(fd, view, range, &[], None)
  }

  /// Submit a non-blocking `write(2)` using pinned `ArrayBuffer`/`TypedArray` backing stores.
  ///
  /// The caller supplies a [`super::PinnedIoVec`] (which owns backing-store pin guards). The pinned
  /// buffers are charged against the I/O limiter and are released on completion/cancellation.
  pub fn write_iovecs(&self, fd: OwnedFd, iovecs: super::PinnedIoVec) -> io::Result<PromiseRef> {
    self.write_iovecs_with_debug_hooks(fd, iovecs, &[], None)
  }

  #[doc(hidden)]
  pub fn write_with_debug_hooks(
    &self,
    fd: OwnedFd,
    view: &Uint8Array,
    range: Range<usize>,
    roots: &[*mut u8],
    debug: Option<IoOpDebugHooks>,
  ) -> io::Result<PromiseRef> {
    if self.inner.state() != RuntimeState::Running {
      return Err(io::Error::new(io::ErrorKind::Other, "I/O runtime is torn down"));
    }

    let promise = async_rt::promise::promise_new();
    let cancel = super::op_registry::CancellationToken::new()?;

    let pinned = super::op::IoOp::pin_uint8_array_range(&self.inner.limiter, view, range)?;
    let root_pins = roots.iter().copied().map(RootPin::new).collect::<Vec<_>>();

    let (id, op) = {
      let mut reg = self.inner.registry.lock();
      let id = reg.alloc_id();
      let op = Arc::new(IoOpRecord::new(
        id,
        IoOpKind::Write { fd },
        pinned,
        promise,
        cancel,
        root_pins,
        debug,
      ));
      reg.insert(Arc::clone(&op));
      (id, op)
    };

    let rt_weak = Arc::downgrade(&self.inner);
    let spawn_res = std::thread::Builder::new()
      .name(format!("rt-io-op-{}", id.as_u64()))
      .spawn(move || io_worker(op, rt_weak));

    match spawn_res {
      Ok(_) => Ok(promise),
      Err(e) => {
        // Best-effort cleanup: if the thread wasn't spawned, remove the op from the registry.
        // Drop outside the registry lock: dropping an op record can unregister GC root pins.
        let removed = self.inner.registry.lock().remove(id);
        drop(removed);
        Err(io::Error::new(io::ErrorKind::Other, e))
      }
    }
  }

  #[doc(hidden)]
  pub fn write_iovecs_with_debug_hooks(
    &self,
    fd: OwnedFd,
    iovecs: super::PinnedIoVec,
    roots: &[*mut u8],
    debug: Option<IoOpDebugHooks>,
  ) -> io::Result<PromiseRef> {
    if self.inner.state() != RuntimeState::Running {
      return Err(io::Error::new(io::ErrorKind::Other, "I/O runtime is torn down"));
    }

    let promise = async_rt::promise::promise_new();
    let cancel = super::op_registry::CancellationToken::new()?;

    let pinned = super::op::IoOp::pin_iovecs(&self.inner.limiter, iovecs)?;
    let root_pins = roots.iter().copied().map(RootPin::new).collect::<Vec<_>>();

    let (id, op) = {
      let mut reg = self.inner.registry.lock();
      let id = reg.alloc_id();
      let op = Arc::new(IoOpRecord::new(
        id,
        IoOpKind::Write { fd },
        pinned,
        promise,
        cancel,
        root_pins,
        debug,
      ));
      reg.insert(Arc::clone(&op));
      (id, op)
    };

    let rt_weak = Arc::downgrade(&self.inner);
    let spawn_res = std::thread::Builder::new()
      .name(format!("rt-io-op-{}", id.as_u64()))
      .spawn(move || io_worker(op, rt_weak));

    match spawn_res {
      Ok(_) => Ok(promise),
      Err(e) => {
        // Best-effort cleanup: if the thread wasn't spawned, remove the op from the registry.
        // Drop outside the registry lock: dropping an op record can unregister GC root pins.
        let removed = self.inner.registry.lock().remove(id);
        drop(removed);
        Err(io::Error::new(io::ErrorKind::Other, e))
      }
    }
  }

  #[doc(hidden)]
  pub fn read_with_debug_hooks(
    &self,
    fd: OwnedFd,
    view: &Uint8Array,
    range: Range<usize>,
    roots: &[*mut u8],
    debug: Option<IoOpDebugHooks>,
  ) -> io::Result<PromiseRef> {
    if self.inner.state() != RuntimeState::Running {
      return Err(io::Error::new(io::ErrorKind::Other, "I/O runtime is torn down"));
    }

    let promise = async_rt::promise::promise_new();
    let cancel = super::op_registry::CancellationToken::new()?;

    let pinned = super::op::IoOp::pin_uint8_array_range_for_read(&self.inner.limiter, view, range)?;
    let root_pins = roots.iter().copied().map(RootPin::new).collect::<Vec<_>>();

    let (id, op) = {
      let mut reg = self.inner.registry.lock();
      let id = reg.alloc_id();
      let op = Arc::new(IoOpRecord::new(
        id,
        IoOpKind::Read { fd },
        pinned,
        promise,
        cancel,
        root_pins,
        debug,
      ));
      reg.insert(Arc::clone(&op));
      (id, op)
    };

    let rt_weak = Arc::downgrade(&self.inner);
    let spawn_res = std::thread::Builder::new()
      .name(format!("rt-io-op-{}", id.as_u64()))
      .spawn(move || io_worker(op, rt_weak));

    match spawn_res {
      Ok(_) => Ok(promise),
      Err(e) => {
        // Best-effort cleanup: if the thread wasn't spawned, remove the op from the registry.
        let _ = self.inner.registry.lock().remove(id);
        Err(io::Error::new(io::ErrorKind::Other, e))
      }
    }
  }

  /// Test-only helper: number of operations currently present in the registry.
  #[doc(hidden)]
  pub fn debug_registry_len(&self) -> usize {
    self.inner.registry.lock().len()
  }

  /// Test-only hook: execute `f` while holding the operation registry lock.
  ///
  /// This exists to deterministically force contention on the registry mutex for
  /// stop-the-world safepoint tests.
  #[doc(hidden)]
  pub fn debug_with_registry_lock<R>(&self, f: impl FnOnce() -> R) -> R {
    let _guard = self.inner.registry.lock();
    f()
  }

  /// Test-only helper: snapshot current limiter counters.
  #[doc(hidden)]
  pub fn debug_counters(&self) -> IoCounters {
    self.inner.limiter.counters()
  }
}

impl Default for IoRuntime {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for IoRuntime {
  fn drop(&mut self) {
    self.teardown();
  }
}

fn io_worker(op: Arc<IoOpRecord>, rt: Weak<IoRuntimeInner>) {
  threading::register_current_thread(ThreadKind::Io);
  struct Unregister;
  impl Drop for Unregister {
    fn drop(&mut self) {
      threading::unregister_current_thread();
    }
  }
  let _unregister = Unregister;

  let out = run_io(&op);
  op.set_outcome(out);

  if let Some(rt_strong) = rt.upgrade() {
    if rt_strong.state() == RuntimeState::Running {
      IoRuntimeInner::enqueue_completion(Arc::downgrade(&rt_strong), op.id());
    } else {
      // Drop outside the registry lock: dropping an op record can unregister GC root pins.
      let removed = rt_strong.registry.lock().remove(op.id());
      drop(removed);
    }
  }

  if let Some(debug) = &op.debug {
    debug.pause_finish_now();
  }
}

fn run_io(op: &IoOpRecord) -> IoOpOutcome {
  // This thread performs syscalls only; mark it GC-safe so stop-the-world GC doesn't have to wake a
  // blocked poll.
  let _gc_safe = threading::enter_gc_safe_region();

  if op.cancel.is_cancelled() {
    return IoOpOutcome::Canceled;
  }

  let data_fd = op.kind.raw_fd();
  if let Err(err) = set_nonblocking(data_fd) {
    return IoOpOutcome::Err(err.raw_os_error().unwrap_or(libc::EIO));
  }

  let cancel_fd = op.cancel.poll_fd();
  let mut fds = [
    libc::pollfd {
      fd: data_fd,
      events: op.kind.poll_events(),
      revents: 0,
    },
    libc::pollfd {
      fd: cancel_fd,
      events: libc::POLLIN,
      revents: 0,
    },
  ];

  let bufs = op.pinned.bufs();
  match &op.kind {
    IoOpKind::Write { .. } => {
      let mut buf_idx: usize = 0;
      let mut offset: usize = 0;
      let mut total_written: usize = 0;

      while buf_idx < bufs.len() {
        // Skip empty segments.
        while buf_idx < bufs.len() && bufs[buf_idx].len() == 0 {
          buf_idx += 1;
          offset = 0;
        }
        if buf_idx >= bufs.len() {
          break;
        }

        let buf = bufs[buf_idx];
        debug_assert!(offset <= buf.len());
        if offset == buf.len() {
          buf_idx += 1;
          offset = 0;
          continue;
        }

        if op.cancel.is_cancelled() {
          return IoOpOutcome::Canceled;
        }

        let rc = unsafe {
          libc::write(
            data_fd,
            buf.as_ptr().wrapping_add(offset) as *const libc::c_void,
            buf.len() - offset,
          )
        };

        if rc > 0 {
          let n = rc as usize;
          offset += n;
          total_written = total_written.saturating_add(n);
          continue;
        }

        if rc == 0 {
          // `write` returning 0 with a non-zero count is unexpected; treat as I/O error.
          return IoOpOutcome::Err(libc::EIO);
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }

        if err.kind() != io::ErrorKind::WouldBlock {
          return IoOpOutcome::Err(err.raw_os_error().unwrap_or(libc::EIO));
        }

        // EAGAIN: wait for POLLOUT or cancellation.
        loop {
          fds[0].revents = 0;
          fds[1].revents = 0;

          let poll_rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
          if poll_rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
              continue;
            }
            return IoOpOutcome::Err(err.raw_os_error().unwrap_or(libc::EIO));
          }

          if fds[1].revents != 0 {
            op.cancel.drain();
            return IoOpOutcome::Canceled;
          }

          if fds[0].revents != 0 {
            break;
          }
        }
      }

      IoOpOutcome::Ok(total_written)
    }
    IoOpKind::Read { .. } => {
      let mut buf_idx: usize = 0;
      let mut offset: usize = 0;
      let mut total_read: usize = 0;

      'outer: while buf_idx < bufs.len() {
        // Skip empty segments.
        while buf_idx < bufs.len() && bufs[buf_idx].len() == 0 {
          buf_idx += 1;
          offset = 0;
        }
        if buf_idx >= bufs.len() {
          break;
        }

        let buf = bufs[buf_idx];
        debug_assert!(offset <= buf.len());
        if offset == buf.len() {
          buf_idx += 1;
          offset = 0;
          continue;
        }

        if op.cancel.is_cancelled() {
          return IoOpOutcome::Canceled;
        }

        let rc = unsafe {
          libc::read(
            data_fd,
            buf.as_mut_ptr().wrapping_add(offset) as *mut libc::c_void,
            buf.len() - offset,
          )
        };

        if rc > 0 {
          let n = rc as usize;
          offset += n;
          total_read = total_read.saturating_add(n);
          continue;
        }

        if rc == 0 {
          // EOF.
          break 'outer;
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }

        if err.kind() != io::ErrorKind::WouldBlock {
          return IoOpOutcome::Err(err.raw_os_error().unwrap_or(libc::EIO));
        }

        // EAGAIN: wait for POLLIN or cancellation.
        loop {
          fds[0].revents = 0;
          fds[1].revents = 0;

          let poll_rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
          if poll_rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
              continue;
            }
            return IoOpOutcome::Err(err.raw_os_error().unwrap_or(libc::EIO));
          }

          if fds[1].revents != 0 {
            op.cancel.drain();
            return IoOpOutcome::Canceled;
          }

          if fds[0].revents != 0 {
            break;
          }
        }
      }

      IoOpOutcome::Ok(total_read)
    }
  }
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = loop {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };
  loop {
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc >= 0 {
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

#[cfg(not(unix))]
fn set_nonblocking(_fd: RawFd) -> io::Result<()> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "nonblocking I/O is only supported on unix platforms",
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn io_runtime_registry_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    const TIMEOUT: Duration = Duration::from_secs(2);

    let rt = Arc::new(IoRuntimeInner {
      state: AtomicU8::new(RuntimeState::Running as u8),
      registry: GcAwareMutex::new(OpRegistry::new()),
      limiter: Arc::new(IoLimiter::new(IoLimits::default())),
    });

    std::thread::scope(|scope| {
      // Thread A holds the registry lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to acquire the registry lock while it is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<usize>();

      let rt_a = Arc::clone(&rt);
      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = rt_a.registry.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the registry lock");

      let rt_c = Arc::clone(&rt);
      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();
        c_start_rx.recv().unwrap();

        let len = rt_c.registry.lock().len();
        c_done_tx.send(len).unwrap();

        threading::unregister_current_thread();
      });

      let c_id = c_registered_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should register with the thread registry");

      // Ensure thread C is actively contending on the registry lock before starting STW.
      c_start_tx.send(()).unwrap();

      // Wait until thread C is marked NativeSafe (this is what prevents STW deadlocks).
      let start = Instant::now();
      loop {
        let mut native_safe = false;
        threading::registry::for_each_thread(|t| {
          if t.id() == c_id {
            native_safe = t.is_native_safe();
          }
        });

        if native_safe {
          break;
        }
        if start.elapsed() > TIMEOUT {
          panic!("thread C did not enter a GC-safe region while blocked on the registry lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      struct ResumeOnDrop;
      impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
          crate::threading::safepoint::rt_gc_resume_world();
        }
      }
      let _resume = ResumeOnDrop;

      // Let thread A release the lock and reach the safepoint.
      a_release_tx.send(()).unwrap();

      assert!(
        crate::threading::safepoint::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
        "world failed to stop within timeout; registry lock contention must not block STW"
      );

      // Resume the world so the contending lock acquisition can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      let len = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("thread C should finish after world is resumed");
      assert_eq!(len, 0);
    });
  }
}
