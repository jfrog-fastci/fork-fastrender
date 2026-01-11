use crate::arch::SafepointContext;
use crate::threading::safepoint;
use std::collections::HashMap;
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Debug, Default, Clone)]
struct HandleStack(Vec<*mut *mut u8>);

// SAFETY: The handle stack is only mutated by the owning thread, and is only
// read by the GC while the world is stopped. The raw pointers are treated as
// opaque addresses; correct usage requires higher-level synchronization.
unsafe impl Send for HandleStack {}

/// Runtime-assigned thread id (stable for the lifetime of a registered thread).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ThreadId(u64);

impl ThreadId {
  pub fn get(self) -> u64 {
    self.0
  }
}

/// Class of thread from the runtime's perspective.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadKind {
  Main = 0,
  Worker = 1,
  Io = 2,
  External = 3,
}

impl ThreadKind {
  fn from_u8(value: u8) -> Self {
    match value {
      0 => Self::Main,
      1 => Self::Worker,
      2 => Self::Io,
      3 => Self::External,
      _ => Self::External,
    }
  }
}

/// Optional stack bounds metadata for precise stack scanning.
///
/// This is populated on platforms where we can query thread stack bounds (Linux
/// via `pthread_getattr_np`). It is used by future precise GC stack scanning and
/// by tests that validate safepoint context capture.
#[derive(Clone, Copy, Debug)]
pub struct StackBounds {
  pub lo: usize,
  pub hi: usize,
}

/// Per-thread state visible to the GC and runtime coordinator.
#[derive(Debug)]
pub struct ThreadState {
  id: ThreadId,
  kind: AtomicU8,

  /// OS thread id (best-effort; used for debugging and diagnostics).
  os_thread_id: u64,

  /// Nesting depth of "GC-safe / native" regions.
  ///
  /// While this is non-zero, the safepoint coordinator treats the thread as
  /// already safe for stop-the-world requests. The thread must not touch/mutate
  /// the managed heap until it exits the region (depth returns to 0).
  pub(crate) native_safe_depth: AtomicUsize,

  /// Whether this thread is currently parked/idle inside the runtime.
  parked: AtomicBool,

  detached: AtomicBool,

  /// The last GC safepoint epoch observed by this thread.
  safepoint_epoch_observed: AtomicU64,

  /// Captured mutator context for the most recent safepoint slow path entry.
  ///
  /// This is published *before* updating `safepoint_epoch_observed` so the GC can safely read it
  /// after observing the epoch barrier.
  safepoint_context: Mutex<Option<SafepointContext>>,

  stack_bounds: Mutex<Option<StackBounds>>,

  /// Per-thread handle stack for temporary roots created by runtime-native Rust
  /// code (not covered by LLVM stackmaps).
  ///
  /// This is intentionally stored in the `ThreadState` so the GC can enumerate
  /// these roots while the world is stopped.
  handle_stack: Mutex<HandleStack>,
}

impl ThreadState {
  pub fn id(&self) -> ThreadId {
    self.id
  }

  pub fn is_native_safe(&self) -> bool {
    self.native_safe_depth.load(Ordering::Acquire) != 0
  }

  pub fn kind(&self) -> ThreadKind {
    ThreadKind::from_u8(self.kind.load(Ordering::Acquire))
  }

  pub fn os_thread_id(&self) -> u64 {
    self.os_thread_id
  }

  pub fn is_parked(&self) -> bool {
    self.parked.load(Ordering::Acquire)
  }

  pub fn is_detached(&self) -> bool {
    self.detached.load(Ordering::Acquire)
  }

  pub fn safepoint_epoch_observed(&self) -> u64 {
    self.safepoint_epoch_observed.load(Ordering::Acquire)
  }

  pub fn stack_bounds(&self) -> Option<StackBounds> {
    *self.stack_bounds.lock().unwrap()
  }

  pub fn safepoint_context(&self) -> Option<SafepointContext> {
    *self.safepoint_context.lock().unwrap()
  }

  pub(crate) fn handle_stack_len(&self) -> usize {
    self.handle_stack.lock().unwrap().0.len()
  }

  pub(crate) fn handle_stack_push(&self, slot: *mut *mut u8) {
    self.handle_stack.lock().unwrap().0.push(slot);
  }

  pub(crate) fn handle_stack_truncate(&self, len: usize) {
    self.handle_stack.lock().unwrap().0.truncate(len);
  }

  pub(crate) fn handle_stack_pop_debug(&self, expected: *mut *mut u8) {
    let mut stack = self.handle_stack.lock().unwrap();
    #[cfg(debug_assertions)]
    {
      let top = stack.0.last().copied();
      debug_assert_eq!(
        top,
        Some(expected),
        "rt_root_pop must be called in strict LIFO order"
      );
    }
    stack.0.pop();
  }

  pub(crate) fn handle_stack_pop_checked(&self, expected: *mut *mut u8) {
    let mut stack = self.handle_stack.lock().unwrap();
    let top = stack.0.last().copied();
    assert_eq!(
      top,
      Some(expected),
      "roots must be dropped/popped in strict LIFO order"
    );
    stack.0.pop();
  }

  pub(crate) fn for_each_handle_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    // GC must not allocate, so avoid cloning the Vec. Copy out one slot pointer at a time under
    // the mutex, then invoke the callback after releasing the lock.
    let mut idx = 0usize;
    loop {
      let Some(slot) = self.handle_stack.lock().unwrap().0.get(idx).copied() else {
        break;
      };
      f(slot);
      idx += 1;
    }
  }
}

/// Snapshot counts of threads by kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadCounts {
  pub main: usize,
  pub worker: usize,
  pub io: usize,
  pub external: usize,
  pub total: usize,
}

struct ThreadRegistry {
  next_id: AtomicU64,
  threads: Mutex<HashMap<ThreadId, Arc<ThreadState>>>,
}

impl ThreadRegistry {
  fn new() -> Self {
    Self {
      next_id: AtomicU64::new(1),
      threads: Mutex::new(HashMap::new()),
    }
  }

  fn register_current_thread(&self, kind: ThreadKind) -> Arc<ThreadState> {
    // Idempotent: allow callers to "ensure registered" without double-registering.
    if let Some(existing) = current_thread_state() {
      // Allow callers to upgrade an "unknown/external" thread into a more
      // specific kind (e.g. main thread first observed via a parallel call, then
      // later via `rt_async_poll`).
      if existing.kind() == ThreadKind::External && kind != ThreadKind::External {
        existing.kind.store(kind as u8, Ordering::Release);
        safepoint::notify_state_change();
      }
      return existing;
    }

    // Avoid claiming we've "observed" an in-progress stop-the-world request:
    // a newly-registered mutator hasn't yet reached a safepoint for the current epoch.
    let global_epoch = safepoint::current_epoch();
    let initial_observed = if global_epoch & 1 == 0 {
      global_epoch
    } else {
      global_epoch.saturating_sub(1)
    };

    let id = ThreadId(self.next_id.fetch_add(1, Ordering::Relaxed));
    let state = Arc::new(ThreadState {
      id,
      kind: AtomicU8::new(kind as u8),
      os_thread_id: current_os_thread_id(),
      native_safe_depth: AtomicUsize::new(0),
      parked: AtomicBool::new(false),
      detached: AtomicBool::new(false),
      safepoint_epoch_observed: AtomicU64::new(initial_observed),
      safepoint_context: Mutex::new(None),
      stack_bounds: Mutex::new(current_stack_bounds()),
      handle_stack: Mutex::new(HandleStack::default()),
    });

    {
      let mut threads = self.threads.lock().unwrap();
      threads.insert(id, state.clone());
    }

    set_tls_thread_registration(ThreadRegistration { state: state.clone() });
    safepoint::notify_state_change();

    // If a GC is already in progress, immediately park at the safepoint before
    // running mutator code.
    //
    // Re-read the epoch after registering to avoid a race where we observed an
    // even epoch, then blocked on the registry lock while another thread
    // requested stop-the-world.
    if safepoint::current_epoch() & 1 == 1 {
      safepoint::rt_gc_safepoint();
    }

    state
  }

  fn unregister_thread(&self, id: ThreadId) {
    let mut threads = self.threads.lock().unwrap();
    threads.remove(&id);
  }

  fn all_threads(&self) -> Vec<Arc<ThreadState>> {
    let threads = self.threads.lock().unwrap();
    threads.values().cloned().collect()
  }

  fn counts(&self) -> ThreadCounts {
    let threads = self.threads.lock().unwrap();
    let mut out = ThreadCounts::default();
    out.total = threads.len();
    for t in threads.values() {
      match t.kind() {
        ThreadKind::Main => out.main += 1,
        ThreadKind::Worker => out.worker += 1,
        ThreadKind::Io => out.io += 1,
        ThreadKind::External => out.external += 1,
      }
    }
    out
  }
}

static REGISTRY: OnceLock<ThreadRegistry> = OnceLock::new();

fn registry() -> &'static ThreadRegistry {
  REGISTRY.get_or_init(ThreadRegistry::new)
}

thread_local! {
  static TLS_THREAD_REGISTRATION: RefCell<Option<ThreadRegistration>> = RefCell::new(None);
}

struct ThreadRegistration {
  state: Arc<ThreadState>,
}

impl Drop for ThreadRegistration {
  fn drop(&mut self) {
    self
      .state
      .detached
      .store(true, Ordering::Release);
    registry().unregister_thread(self.state.id);
    safepoint::notify_state_change();
  }
}

fn set_tls_thread_registration(reg: ThreadRegistration) {
  TLS_THREAD_REGISTRATION.with(|cell| {
    *cell.borrow_mut() = Some(reg);
  });
}

fn clear_tls_thread_registration() {
  TLS_THREAD_REGISTRATION.with(|cell| {
    *cell.borrow_mut() = None;
  });
}

/// Return this thread's registered [`ThreadState`], if any.
pub fn current_thread_state() -> Option<Arc<ThreadState>> {
  TLS_THREAD_REGISTRATION.with(|cell| {
    cell
      .borrow()
      .as_ref()
      .map(|reg| reg.state.clone())
  })
}

/// Return this thread's registered [`ThreadId`], if any.
pub fn current_thread_id() -> Option<ThreadId> {
  TLS_THREAD_REGISTRATION.with(|cell| cell.borrow().as_ref().map(|reg| reg.state.id))
}

/// Register the current thread with the global registry.
pub fn register_current_thread(kind: ThreadKind) -> ThreadId {
  // Eagerly parse and index stackmaps the first time any thread registers. This prevents
  // stop-the-world GC/root enumeration from doing the lazy `StackMaps::parse` allocation work
  // while the world is stopped.
  let _ = crate::stackmap::try_stackmaps();
  registry().register_current_thread(kind).id
}

/// Unregister the current thread from the global registry.
pub fn unregister_current_thread() {
  // Fast-path: allow callers to "ensure unregistered" without doing work.
  if current_thread_state().is_none() {
    return;
  }

  loop {
    // GC handshake: if a stop-the-world is in progress (odd epoch), this will
    // enter the safepoint slow path, publish our context, and block until the
    // world is resumed. We must do this *while still registered* so the
    // coordinator cannot forget about this mutator.
    safepoint::rt_gc_safepoint();

    // Prevent racing the GC coordinator while it is iterating thread states /
    // contexts, and serialize against starting a new stop-the-world request.
    let _world_guard = safepoint::gc_world_lock();

    // A stop-the-world request may have started after we returned from the
    // safepoint poll but before acquiring `gc_world_lock`. If so, retry: the
    // next safepoint poll will block until resumed.
    if safepoint::current_epoch() & 1 == 1 {
      continue;
    }

    // Clearing TLS drops the current thread's registration, which removes this
    // thread from the global registry.
    clear_tls_thread_registration();
    break;
  }
}

/// Snapshot all registered threads (for GC iteration).
pub fn all_threads() -> Vec<Arc<ThreadState>> {
  registry().all_threads()
}

/// Iterate all registered threads without allocating.
///
/// The callback is invoked while holding the thread registry lock; it must not call
/// [`register_current_thread`] / [`unregister_current_thread`].
pub fn for_each_thread(mut f: impl FnMut(&Arc<ThreadState>)) {
  let threads = registry().threads.lock().unwrap();
  for thread in threads.values() {
    f(thread);
  }
}

/// Like [`for_each_thread`], but allows fallible iteration.
pub fn try_for_each_thread<E>(mut f: impl FnMut(&Arc<ThreadState>) -> Result<(), E>) -> Result<(), E> {
  let threads = registry().threads.lock().unwrap();
  for thread in threads.values() {
    f(thread)?;
  }
  Ok(())
}

/// Snapshot thread counts by kind.
pub fn thread_counts() -> ThreadCounts {
  registry().counts()
}

/// Mark the current thread as parked/unparked.
pub fn set_current_thread_parked(parked: bool) {
  TLS_THREAD_REGISTRATION.with(|cell| {
    if let Some(reg) = cell.borrow().as_ref() {
      if parked {
        let ctx = crate::arch::capture_safepoint_context();
        set_current_thread_safepoint_context(ctx);
      }
      reg.state.parked.store(parked, Ordering::Release);
      safepoint::notify_state_change();
    }
  });
}

/// Update the current thread's observed safepoint epoch.
pub(crate) fn set_current_thread_safepoint_epoch_observed(epoch: u64) {
  TLS_THREAD_REGISTRATION.with(|cell| {
    if let Some(reg) = cell.borrow().as_ref() {
      reg
        .state
        .safepoint_epoch_observed
        .store(epoch, Ordering::Release);
    }
  });
}

pub(crate) fn set_current_thread_safepoint_context(ctx: SafepointContext) {
  let Some(state) = current_thread_state() else {
    return;
  };

  *state.safepoint_context.lock().unwrap() = Some(ctx);
}

/// Best-effort OS thread id for debugging.
fn current_os_thread_id() -> u64 {
  #[cfg(any(target_os = "linux", target_os = "android"))]
  unsafe {
    libc::syscall(libc::SYS_gettid) as u64
  }

  #[cfg(not(any(target_os = "linux", target_os = "android")))]
  {
    // Fallback: stable but not OS-level.
    // `ThreadId` formatting is intentionally opaque, so we hash its Debug form.
    use std::hash::Hash;
    use std::hash::Hasher;
    let tid = std::thread::current().id();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tid.hash(&mut hasher);
    hasher.finish()
  }
}

fn current_stack_bounds() -> Option<StackBounds> {
  let bounds = crate::thread_stack::current_thread_stack_bounds().ok()?;
  Some(StackBounds {
    lo: bounds.low,
    hi: bounds.high,
  })
}
