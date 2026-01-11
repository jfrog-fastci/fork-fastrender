use crate::runtime::Runtime;
use std::cell::Cell;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

// TLS pointer to the currently-attached [`Thread`].
thread_local! {
  static RT_THREAD: Cell<*mut Thread> = Cell::new(std::ptr::null_mut());
}

/// Get the raw TLS pointer to the current thread record.
pub fn current_thread_ptr() -> *mut Thread {
  RT_THREAD.with(|ptr| ptr.get())
}

/// Returns the current thread record if the calling OS thread is attached.
pub fn current_thread() -> Option<&'static Thread> {
  unsafe { current_thread_ptr().as_ref() }
}

/// Returns a mutable reference to the current thread record.
///
/// # Safety
/// This is not safe to expose as a safe API because callers can create multiple
/// mutable references (or a mutable reference while holding an immutable one)
/// to the same thread record by calling this function repeatedly and/or
/// combining it with [`current_thread`].
pub unsafe fn current_thread_mut() -> Option<&'static mut Thread> {
  current_thread_ptr().as_mut()
}

/// Returns the current thread state, or [`ThreadState::Detached`] if not
/// attached.
pub fn current_thread_state() -> ThreadState {
  current_thread()
    .map(|t| t.state())
    .unwrap_or(ThreadState::Detached)
}

/// Install `thread` in TLS.
///
/// # Safety
/// Must only be called by the runtime during attach/detach.
pub unsafe fn set_current_thread_ptr(thread: *mut Thread) {
  RT_THREAD.with(|ptr| ptr.set(thread));
}

/// Per-mutator thread record.
///
/// This structure is `repr(C)` because native codegen will compute field offsets
/// directly.
#[repr(C)]
pub struct Thread {
  pub id: u32,
  pub os_tid: u64,
  pub stack_lo: usize,
  pub stack_hi: usize,

  pub state: AtomicU8,

  pub local_epoch: AtomicU64,

  // Published statepoint context. The JIT will update these at safepoints so the
  // GC can scan stacks using LLVM stackmaps.
  pub sp: AtomicUsize,
  pub fp: AtomicUsize,
  pub ip: AtomicUsize,

  // Owning runtime (opaque to C/native code). This enables a `Thread*`-only
  // detach API.
  pub(crate) runtime: *const Runtime,
}

impl Thread {
  pub(crate) fn new(runtime: &Runtime, id: u32, os_tid: u64, stack_lo: usize, stack_hi: usize) -> Self {
    Self {
      id,
      os_tid,
      stack_lo,
      stack_hi,
      state: AtomicU8::new(ThreadState::Running as u8),
      local_epoch: AtomicU64::new(0),
      sp: AtomicUsize::new(0),
      fp: AtomicUsize::new(0),
      ip: AtomicUsize::new(0),
      runtime: runtime as *const Runtime,
    }
  }

  pub fn state(&self) -> ThreadState {
    ThreadState::from_u8(self.state.load(Ordering::Acquire))
  }

  pub fn set_state(&self, state: ThreadState) {
    self.state.store(state as u8, Ordering::Release);
  }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
  Running = 0,
  Parked = 1,
  NativeSafe = 2,
  Detached = 3,
}

impl ThreadState {
  fn from_u8(v: u8) -> Self {
    match v {
      0 => ThreadState::Running,
      1 => ThreadState::Parked,
      2 => ThreadState::NativeSafe,
      3 => ThreadState::Detached,
      _ => ThreadState::Detached,
    }
  }
}

pub(crate) fn current_os_tid() -> u64 {
  #[cfg(target_os = "linux")]
  unsafe {
    libc::syscall(libc::SYS_gettid) as u64
  }

  #[cfg(not(target_os = "linux"))]
  {
    // Best-effort fallback for platforms where `gettid` is unavailable.
    //
    // We intentionally avoid `std::thread::ThreadId::as_u64()` here because it is
    // unstable; `pthread_self()` is portable across Unix-like platforms and is
    // sufficient as a unique identifier while the thread is alive.
    unsafe { libc::pthread_self() as usize as u64 }
  }
}

pub(crate) fn current_stack_bounds() -> (usize, usize) {
  if let Ok(bounds) = crate::thread_stack::current_thread_stack_bounds() {
    return (bounds.low, bounds.high);
  }

  // Fallback: estimate bounds around the current stack pointer. This is only
  // used when stack introspection fails.
  let mut dummy = 0u8;
  let sp = std::ptr::addr_of_mut!(dummy) as usize;
  const GUESS: usize = 8 * 1024 * 1024;
  let lo = sp.saturating_sub(GUESS);
  let hi = sp.saturating_add(GUESS);
  (lo.min(hi), lo.max(hi))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn thread_layout_is_stable() {
    use std::mem::{align_of, offset_of, size_of};

    // The runtime/native codegen contract currently assumes 64-bit pointers.
    assert_eq!(size_of::<usize>(), 8);

    // `Thread` is `repr(C)` and codegen is expected to hardcode field offsets.
    assert_eq!(align_of::<Thread>(), 8);
    assert_eq!(size_of::<Thread>(), 80);

    assert_eq!(offset_of!(Thread, id), 0);
    assert_eq!(offset_of!(Thread, os_tid), 8);
    assert_eq!(offset_of!(Thread, stack_lo), 16);
    assert_eq!(offset_of!(Thread, stack_hi), 24);
    assert_eq!(offset_of!(Thread, state), 32);
    assert_eq!(offset_of!(Thread, local_epoch), 40);
    assert_eq!(offset_of!(Thread, sp), 48);
    assert_eq!(offset_of!(Thread, fp), 56);
    assert_eq!(offset_of!(Thread, ip), 64);
    assert_eq!(offset_of!(Thread, runtime), 72);
  }
}
