use crate::thread;
use crate::Thread;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// Global runtime object.
///
/// At the moment this only manages thread attachment and provides a registry of
/// all threads that are currently attached to the runtime.
pub struct Runtime {
  // Read-locked during normal execution. Future GC will take the write-lock to
  // establish a stop-the-world phase where the thread registry can be iterated
  // without concurrent attach/detach.
  world_lock: RwLock<()>,

  registry: Mutex<Registry>,

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
      world_lock: RwLock::new(()),
      registry: Mutex::new(Registry::default()),
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
    Ok(ThreadGuard { runtime: self, thread })
  }

  /// Low-level attach used by the C ABI: attaches and returns the raw pointer.
  ///
  /// Unlike [`Self::attach_current_thread`], the caller is responsible for
  /// eventually calling detach.
  pub fn attach_current_thread_raw(&self) -> Result<*mut Thread, AttachError> {
    if !thread::current_thread_ptr().is_null() {
      return Err(AttachError::AlreadyAttached);
    }

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

    let thread: &'static mut Thread = Box::leak(Box::new(Thread::new(self, id, os_tid, stack_lo, stack_hi)));
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

    // Mark detached before clearing TLS (so other code that might have kept a
    // reference can observe the state change).
    (*thread).set_state(crate::ThreadState::Detached);

    thread::set_current_thread_ptr(std::ptr::null_mut());

    Ok(())
  }

  /// Enter a stop-the-world phase.
  ///
  /// While the returned guard is alive, thread attach/detach is blocked.
  ///
  /// This is only a placeholder; future work will also coordinate safepoints so
  /// that all threads are parked before GC scans stacks.
  pub fn stop_the_world(&self) -> StopTheWorldGuard<'_> {
    StopTheWorldGuard {
      _guard: self.world_lock.write(),
    }
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
