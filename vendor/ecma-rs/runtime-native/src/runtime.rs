use crate::sync::{GcAwareMutex, GcAwareRwLock};
use crate::thread;
use crate::threading::safepoint::StopReason;
use crate::threading::{self, ThreadKind};
use crate::Thread;
use std::sync::atomic::{AtomicU32, Ordering};

/// Global runtime object.
///
/// At the moment this only manages `rt_thread_attach` / `rt_thread_detach` and
/// provides a registry of all threads currently attached to this `Runtime`
/// instance.
///
/// Note: stop-the-world GC safepoints are coordinated by the process-global
/// thread registry in [`crate::threading`], not by this per-runtime registry.
pub struct Runtime {
  // Read-locked during normal execution. Used to establish stop-the-world phases where the thread
  // registry can be iterated without concurrent attach/detach.
  //
  // This is a GC-aware lock: contended acquisition temporarily enters a GC-safe ("native") region
  // while blocked so global stop-the-world safepoints don't deadlock on threads waiting for this
  // lock.
  //
  // Note: global GC stop-the-world safepoints are coordinated by the process-global thread
  // registry in `crate::threading`, not by this per-runtime lock.
  world_lock: GcAwareRwLock<()>,

  // Likewise, the registry mutex is GC-aware so contended attach/detach can be treated as
  // quiescent for STW coordination.
  registry: GcAwareMutex<Registry>,

  next_thread_id: AtomicU32,
}

#[derive(Default)]
struct Registry {
  threads: Vec<*mut Thread>,
}

// `Registry` is always accessed behind `Runtime::registry`'s mutex.
//
// Raw pointers are intentionally used because the thread records are leaked (so
// their addresses are stable) and will be accessed from generated/native code
// via FFI. The pointers themselves are safe to move between threads.
unsafe impl Send for Registry {}

impl Runtime {
  pub fn new() -> Self {
    Self {
      world_lock: GcAwareRwLock::new(()),
      registry: GcAwareMutex::new(Registry::default()),
      // Start IDs at 1 so 0 can be used as a sentinel in native code if needed.
      next_thread_id: AtomicU32::new(1),
    }
  }

  /// Number of currently attached threads.
  pub fn thread_count(&self) -> usize {
    self.registry.lock().threads.len()
  }

  /// Attach the current OS thread to this runtime and return an RAII guard.
  pub fn attach_current_thread(&self) -> Result<ThreadGuard<'_>, AttachError> {
    let thread = self.attach_current_thread_raw()?;
    Ok(ThreadGuard {
      runtime: self,
      thread,
      _not_send: std::marker::PhantomData,
    })
  }

  /// Low-level attach used by the C ABI: attaches and returns the raw pointer.
  ///
  /// Unlike [`Self::attach_current_thread`], the caller is responsible for
  /// eventually calling detach.
  pub fn attach_current_thread_raw(&self) -> Result<*mut Thread, AttachError> {
    if !thread::current_thread_ptr().is_null() {
      return Err(AttachError::AlreadyAttached);
    }

    // Determine whether we "own" the global registration: if the thread is already registered (e.g.
    // via `rt_thread_init` / `rt_thread_register`), `detach` must *not* unregister it.
    let was_registered = threading::registry::current_thread_id().is_some();

    // Ensure this OS thread participates in the global GC safepoint protocol.
    //
    // This is idempotent, and if a stop-the-world is already active it will park
    // before returning (so the thread cannot start mutator work mid-STW).
    threading::register_current_thread(ThreadKind::External);

    // Avoid mutating the thread registry while a stop-the-world safepoint is in
    // progress.
    //
    // Important: use the *cooperative* safepoint poll instead of
    // `wait_while_stop_the_world`. The current thread may itself be a registered
    // mutator, in which case blocking without first acknowledging the safepoint
    // request could deadlock the STW coordinator.
    crate::threading::safepoint_poll();

    // Prevent attach/detach while a stop-the-world phase is active.
    let _world = self.world_lock.read();

    let id = self.next_thread_id.fetch_add(1, Ordering::Relaxed);
    let (stack_lo, stack_hi) = thread::current_stack_bounds();
    let os_tid = thread::current_os_tid();

    let thread: &'static mut Thread = Box::leak(Box::new(Thread::new(
      self,
      id,
      os_tid,
      stack_lo,
      stack_hi,
      !was_registered,
    )));
    let thread_ptr = thread as *mut Thread;

    {
      let mut reg = self.registry.lock();
      reg.threads.push(thread_ptr);
    }

    // Publish in TLS last, so failure before registration doesn't leave a
    // dangling TLS pointer.
    unsafe { thread::set_current_thread_ptr(thread_ptr) };

    Ok(thread_ptr)
  }

  /// Detach the current OS thread from this runtime.
  pub fn detach_current_thread(&self) -> Result<(), DetachError> {
    let thread = thread::current_thread_ptr();
    if thread.is_null() {
      return Err(DetachError::NotAttached);
    }
    unsafe { self.detach_thread_ptr(thread) }
  }

  /// Detach `thread` from this runtime.
  ///
  /// This must only be called on the OS thread that owns `thread` (i.e. the
  /// thread that has it installed in TLS).
  pub unsafe fn detach_thread_ptr(&self, thread: *mut Thread) -> Result<(), DetachError> {
    if thread.is_null() {
      return Err(DetachError::NotAttached);
    }

    // Avoid mutating the thread registry while a stop-the-world safepoint is in
    // progress. See `attach_current_thread_raw` for why we use the cooperative
    // safepoint poll here.
    crate::threading::safepoint_poll();

    let unregister_global;
    {
      // Prevent detach while a stop-the-world phase is active.
      let _world = self.world_lock.read();

      // Ensure this is the current thread (TLS). This keeps the API honest and
      // avoids accidentally clearing some other thread's TLS.
      if thread::current_thread_ptr() != thread {
        return Err(DetachError::NotCurrentThread);
      }

      {
        let mut reg = self.registry.lock();
        if let Some(pos) = reg.threads.iter().position(|&t| t == thread) {
          reg.threads.swap_remove(pos);
        } else {
          return Err(DetachError::NotInRegistry);
        }
      }

      unregister_global = (*thread).registered_by_attach;

      // Mark detached before clearing TLS (so other code that might have kept a
      // reference can observe the state change).
      (*thread).set_state(crate::ThreadState::Detached);

      thread::set_current_thread_ptr(std::ptr::null_mut());
    }

    if unregister_global {
      // Unregister from the global GC thread registry only if `attach` performed the registration.
      threading::unregister_current_thread();
    }

    Ok(())
  }

  /// Enter a stop-the-world phase.
  ///
  /// While the returned guard is alive, thread attach/detach is blocked.
  ///
  /// This only blocks attach/detach to this [`Runtime`]; use [`Self::stop_the_world`] to also
  /// coordinate global GC safepoints.
  pub fn stop_the_world_guard(&self) -> StopTheWorldGuard<'_> {
    StopTheWorldGuard {
      _guard: self.world_lock.write(),
    }
  }

  /// Run `f` under a global stop-the-world GC safepoint, while also blocking thread attach/detach
  /// for this [`Runtime`].
  pub fn stop_the_world<F, R>(&self, reason: StopReason, f: F) -> R
  where
    F: FnOnce() -> R,
  {
    // Block attach/detach while the world is stopped.
    let _world = self.world_lock.write();
    crate::threading::safepoint::stop_the_world(reason, f)
  }

  /// Iterate all attached threads while in a stop-the-world phase.
  pub fn with_attached_threads_stw<R>(&self, _stw: &StopTheWorldGuard<'_>, f: impl FnOnce(&[*mut Thread]) -> R) -> R {
    let reg = self.registry.lock();
    f(&reg.threads)
  }
}

impl Default for Runtime {
  fn default() -> Self {
    Self::new()
  }
}

pub struct ThreadGuard<'a> {
  runtime: &'a Runtime,
  thread: *mut Thread,
  // Not `Send`/`Sync`: this guard owns thread-local attachment state and must be
  // dropped on the thread that created it.
  _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ThreadGuard<'_> {
  pub fn thread_ptr(&self) -> *mut Thread {
    self.thread
  }

  pub fn thread(&self) -> &'static Thread {
    // The thread record is leaked on attach, so it is safe to hand out a
    // `'static` reference. Detach removes it from the registry and clears TLS
    // but does not deallocate it.
    unsafe { &*self.thread }
  }
}

impl Drop for ThreadGuard<'_> {
  fn drop(&mut self) {
    // The guard should only be dropped on the thread it was created on.
    let _ = unsafe { self.runtime.detach_thread_ptr(self.thread) };
  }
}

pub struct StopTheWorldGuard<'a> {
  _guard: parking_lot::RwLockWriteGuard<'a, ()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
  AlreadyAttached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachError {
  NotAttached,
  NotCurrentThread,
  NotInRegistry,
}
