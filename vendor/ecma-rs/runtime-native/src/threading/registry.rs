use crate::threading::safepoint;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Runtime-assigned thread id (stable for the lifetime of a registered thread).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ThreadId(u64);

impl ThreadId {
  pub fn get(self) -> u64 {
    self.0
  }
}

/// Class of thread from the runtime's perspective.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadKind {
  Main,
  Worker,
  Io,
  External,
}

/// Optional stack bounds metadata for precise stack scanning.
///
/// Currently unused, but kept so GC integration can store per-thread stack
/// ranges (and potentially platform-specific metadata) in the registry.
#[derive(Clone, Copy, Debug)]
pub struct StackBounds {
  pub lo: usize,
  pub hi: usize,
}

/// Per-thread state visible to the GC and runtime coordinator.
#[derive(Debug)]
pub struct ThreadState {
  id: ThreadId,
  kind: ThreadKind,

  /// OS thread id (best-effort; used for debugging and diagnostics).
  os_thread_id: u64,

  /// Whether this thread is currently parked/idle inside the runtime.
  parked: AtomicBool,

  /// The last GC safepoint epoch observed by this thread.
  safepoint_epoch_observed: AtomicU64,

  stack_bounds: Mutex<Option<StackBounds>>,
}

impl ThreadState {
  pub fn id(&self) -> ThreadId {
    self.id
  }

  pub fn kind(&self) -> ThreadKind {
    self.kind
  }

  pub fn os_thread_id(&self) -> u64 {
    self.os_thread_id
  }

  pub fn is_parked(&self) -> bool {
    self.parked.load(Ordering::Acquire)
  }

  pub fn safepoint_epoch_observed(&self) -> u64 {
    self.safepoint_epoch_observed.load(Ordering::Acquire)
  }

  pub fn stack_bounds(&self) -> Option<StackBounds> {
    *self.stack_bounds.lock().unwrap()
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
      kind,
      os_thread_id: current_os_thread_id(),
      parked: AtomicBool::new(false),
      safepoint_epoch_observed: AtomicU64::new(initial_observed),
      stack_bounds: Mutex::new(None),
    });

    {
      let mut threads = self.threads.lock().unwrap();
      threads.insert(id, state.clone());
    }

    set_tls_thread_state(state.clone());
    safepoint::notify_state_change();

    // If a GC is already in progress, immediately park at the safepoint before
    // running mutator code.
    if global_epoch & 1 == 1 {
      safepoint::rt_gc_safepoint();
    }

    state
  }

  fn unregister_current_thread(&self) {
    let Some(state) = current_thread_state() else {
      return;
    };

    let id = state.id;
    clear_tls_thread_state();

    {
      let mut threads = self.threads.lock().unwrap();
      if threads.remove(&id).is_some() {
      }
    }

    safepoint::notify_state_change();
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
      match t.kind {
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
  static TLS_THREAD_STATE: std::cell::RefCell<Option<Arc<ThreadState>>> = std::cell::RefCell::new(None);
}

fn set_tls_thread_state(state: Arc<ThreadState>) {
  TLS_THREAD_STATE.with(|cell| {
    *cell.borrow_mut() = Some(state);
  });
}

fn clear_tls_thread_state() {
  TLS_THREAD_STATE.with(|cell| {
    *cell.borrow_mut() = None;
  });
}

/// Return this thread's registered [`ThreadState`], if any.
pub fn current_thread_state() -> Option<Arc<ThreadState>> {
  TLS_THREAD_STATE.with(|cell| cell.borrow().clone())
}

/// Return this thread's registered [`ThreadId`], if any.
pub fn current_thread_id() -> Option<ThreadId> {
  current_thread_state().map(|s| s.id)
}

/// Register the current thread with the global registry.
pub fn register_current_thread(kind: ThreadKind) -> ThreadId {
  registry().register_current_thread(kind).id
}

/// Unregister the current thread from the global registry.
pub fn unregister_current_thread() {
  registry().unregister_current_thread();
}

/// Snapshot all registered threads (for GC iteration).
pub fn all_threads() -> Vec<Arc<ThreadState>> {
  registry().all_threads()
}

/// Snapshot thread counts by kind.
pub fn thread_counts() -> ThreadCounts {
  registry().counts()
}

/// Mark the current thread as parked/unparked.
pub fn set_current_thread_parked(parked: bool) {
  let Some(state) = current_thread_state() else {
    return;
  };

  state.parked.store(parked, Ordering::Release);
  safepoint::notify_state_change();
}

/// Update the current thread's observed safepoint epoch.
pub(crate) fn set_current_thread_safepoint_epoch_observed(epoch: u64) {
  let Some(state) = current_thread_state() else {
    return;
  };

  state.safepoint_epoch_observed.store(epoch, Ordering::Release);
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
