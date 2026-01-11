use crate::abi::PromiseRef;
use crate::threading::ThreadKind;
use crate::{async_rt, threading};
use parking_lot::Mutex;
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
  registry: Mutex<OpRegistry>,
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
        let _ = rt.registry.lock().remove(st.id);
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
        registry: Mutex::new(OpRegistry::new()),
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
  /// (`Arc<[u8]>`), and the bytes are pinned + accounted for via [`super::op::IoOp`].
  pub fn write(&self, fd: OwnedFd, backing: Arc<[u8]>, range: Range<usize>) -> io::Result<PromiseRef> {
    self.write_with_debug_hooks(fd, backing, range, &[], None)
  }

  #[doc(hidden)]
  pub fn write_with_debug_hooks(
    &self,
    fd: OwnedFd,
    backing: Arc<[u8]>,
    range: Range<usize>,
    roots: &[*mut u8],
    debug: Option<IoOpDebugHooks>,
  ) -> io::Result<PromiseRef> {
    if self.inner.state() != RuntimeState::Running {
      return Err(io::Error::new(io::ErrorKind::Other, "I/O runtime is torn down"));
    }

    let promise = async_rt::promise::promise_new();
    let cancel = super::op_registry::CancellationToken::new()?;

    let pinned = super::op::IoOp::pin_range(&self.inner.limiter, backing, range)?;
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
      let _ = rt_strong.registry.lock().remove(op.id());
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
  if bufs.len() != 1 {
    return IoOpOutcome::Err(libc::EINVAL);
  }

  let buf = bufs[0];
  let mut offset: usize = 0;

  while offset < buf.len() {
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
      offset += rc as usize;
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

  IoOpOutcome::Ok(offset)
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

#[cfg(not(unix))]
fn set_nonblocking(_fd: RawFd) -> io::Result<()> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "nonblocking I/O is only supported on unix platforms",
  ))
}
