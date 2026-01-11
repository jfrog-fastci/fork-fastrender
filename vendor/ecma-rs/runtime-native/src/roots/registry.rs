use std::marker::PhantomData;

use crate::sync::GcAwareMutex;

/// A process-global registry of GC root *slots* that are not discovered via
/// LLVM stackmaps (globals, persistent handles, etc).
///
/// Slots are pointers to GC references (`*mut u8`). During collection the GC may
/// update the slot in-place (e.g. when evacuating a young object).
///
/// ## Safety contract
/// Callers must ensure that any registered `slot: *mut *mut u8` remains valid
/// and writable until it is unregistered.
pub struct RootRegistry {
  inner: GcAwareMutex<Inner>,
}

#[derive(Default)]
struct Inner {
  slots: Vec<Slot>,
  free_list: Vec<u32>,
}

#[derive(Default)]
struct Slot {
  generation: u8,
  entry: Option<Entry>,
}

enum Entry {
  Borrowed(*mut *mut u8),
  Pinned(Box<*mut u8>),
}

impl Entry {
  #[inline]
  fn slot_ptr(&mut self) -> *mut *mut u8 {
    match self {
      Entry::Borrowed(ptr) => *ptr,
      Entry::Pinned(b) => b.as_mut() as *mut *mut u8,
    }
  }
}

// SAFETY: The registry stores raw pointers as opaque values. All mutation and
// enumeration are synchronized via the `Mutex`, and the GC only reads/updates
// the pointed-to slots while the world is stopped.
unsafe impl Send for Inner {}

impl RootRegistry {
  pub fn new() -> Self {
    Self {
      inner: GcAwareMutex::new(Inner::default()),
    }
  }

  /// Register a root slot whose storage is owned by the caller.
  ///
  /// The returned handle must later be passed to [`RootRegistry::unregister`].
  pub fn register_root_slot(&self, slot: *mut *mut u8) -> u32 {
    if slot.is_null() {
      std::process::abort();
    }
    if (slot as usize) % core::mem::align_of::<*mut u8>() != 0 {
      std::process::abort();
    }
    let mut inner = self.inner.lock();
    inner.alloc(Entry::Borrowed(slot))
  }

  /// Convenience helper: allocate an internal slot, initialize it to `ptr`, and
  /// register it as a root.
  ///
  /// The returned handle must later be passed to [`RootRegistry::unregister`]
  /// (or the C ABI `rt_gc_unpin`).
  pub fn pin(&self, ptr: *mut u8) -> u32 {
    let mut inner = self.inner.lock();
    inner.alloc(Entry::Pinned(Box::new(ptr)))
  }

  /// Unregister a previously registered root slot handle.
  pub fn unregister(&self, handle: u32) {
    let mut inner = self.inner.lock();
    let _ = inner.remove(handle);
  }

  /// Unregister a previously registered *borrowed* root slot by its address.
  ///
  /// This is useful for APIs that want to register global/static root slots without managing
  /// handle IDs. If the same slot address was registered multiple times, all matching entries are
  /// removed.
  ///
  /// # Panics
  /// Panics if `slot` is null.
  pub fn unregister_root_slot_ptr(&self, slot: *mut *mut u8) {
    if slot.is_null() {
      std::process::abort();
    }
    if (slot as usize) % core::mem::align_of::<*mut u8>() != 0 {
      std::process::abort();
    }
    let mut inner = self.inner.lock();
    inner.remove_borrowed_slot_ptr(slot);
  }

  /// Returns the current pointer value for a registered root handle.
  ///
  /// This can be used by host/async code to re-load a GC pointer after a GC cycle may have
  /// relocated the object.
  ///
  /// Returns `None` if:
  /// - `handle` is invalid,
  /// - `handle` is stale (slot generation mismatch),
  /// - or the entry was already removed.
  ///
  /// # Safety
  /// For handles created by [`RootRegistry::register_root_slot`], the caller must uphold the root
  /// registry contract: the registered slot pointer must remain valid until it is unregistered.
  pub fn get(&self, handle: u32) -> Option<*mut u8> {
    let mut inner = self.inner.lock();
    let slot_ptr = inner.get_slot_ptr(handle)?;
    // Safety: the registry only returns pointers for live entries, and the caller guarantees that
    // borrowed slots remain valid until unregistered.
    Some(unsafe { slot_ptr.read() })
  }

  /// Updates the pointer value stored in a registered root handle.
  ///
  /// Returns `false` if `handle` is invalid/stale/removed.
  ///
  /// # Safety
  /// For handles created by [`RootRegistry::register_root_slot`], the caller must uphold the root
  /// registry contract: the registered slot pointer must remain valid until it is unregistered.
  pub fn set(&self, handle: u32, ptr: *mut u8) -> bool {
    let mut inner = self.inner.lock();
    let Some(slot_ptr) = inner.get_slot_ptr(handle) else {
      return false;
    };
    // Safety: see [`RootRegistry::get`].
    unsafe {
      slot_ptr.write(ptr);
    }
    true
  }

  /// Enumerate all registered root slots.
  pub fn for_each_root_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    // Use the GC-aware `lock()` path so:
    // - contended acquisition enters a GC-safe ("NativeSafe") region (avoids STW deadlocks), and
    // - mutator threads cannot observe a stop-the-world (odd) epoch and still proceed holding this
    //   lock (they will safepoint instead).
    //
    // The GC coordinator can still acquire the lock during stop-the-world: `GcAwareMutex::lock()`
    // treats the coordinator thread as special and returns a guard even while the epoch is odd.
    let mut inner = self.inner.lock();
    for slot in &mut inner.slots {
      let Some(entry) = slot.entry.as_mut() else {
        continue;
      };
      f(entry.slot_ptr());
    }
  }

  /// Test-only helper to reset the process-global registry.
  pub(crate) fn clear_for_tests(&self) {
    let mut inner = self.inner.lock();
    // Important: do *not* truncate `slots` here.
    //
    // Tests frequently clear global runtime state while background worker threads are still
    // unwinding/dropping old handles. If we were to drop the `slots` vector, handle IDs would be
    // immediately reusable from index 0 again with generation 0, allowing a stale `unregister` call
    // from a previous test to accidentally remove a new root registered in the next test.
    //
    // Instead, clear entries in-place and bump each slot's generation to invalidate any previously
    // issued handles.
    inner.free_list.clear();
    for idx in 0..inner.slots.len() {
      {
        let slot = &mut inner.slots[idx];
        slot.entry = None;
        slot.generation = slot.generation.wrapping_add(1);
      }
      // Reuse the slot on the next allocation.
      inner
        .free_list
        .push(u32::try_from(idx).expect("too many root slots"));
    }
  }
}

// Handle encoding:
// - low 24 bits: index+1 (so 0 is invalid)
// - high 8 bits: generation counter
const INDEX_BITS: u32 = 24;
const INDEX_MASK: u32 = (1u32 << INDEX_BITS) - 1;

fn encode_handle(index: u32, generation: u8) -> u32 {
  debug_assert!(index < INDEX_MASK, "root registry index overflow");
  ((generation as u32) << INDEX_BITS) | (index + 1)
}

fn decode_handle(handle: u32) -> Option<(u32, u8)> {
  let index_plus1 = handle & INDEX_MASK;
  if index_plus1 == 0 {
    return None;
  }
  let gen = (handle >> INDEX_BITS) as u8;
  Some((index_plus1 - 1, gen))
}

impl Inner {
  fn alloc(&mut self, entry: Entry) -> u32 {
    if let Some(index) = self.free_list.pop() {
      let slot = &mut self.slots[index as usize];
      debug_assert!(slot.entry.is_none());
      slot.entry = Some(entry);
      encode_handle(index, slot.generation)
    } else {
      let index = u32::try_from(self.slots.len()).expect("too many root slots");
      // Ensure index+1 fits in INDEX_BITS.
      if index >= INDEX_MASK {
        panic!("too many root slots (max {})", INDEX_MASK - 1);
      }
      self.slots.push(Slot {
        generation: 0,
        entry: Some(entry),
      });
      encode_handle(index, 0)
    }
  }

  fn remove(&mut self, handle: u32) -> Option<Entry> {
    let (index, generation) = decode_handle(handle)?;
    let slot = self.slots.get_mut(index as usize)?;
    if slot.generation != generation {
      return None;
    }
    let entry = slot.entry.take()?;
    slot.generation = slot.generation.wrapping_add(1);
    self.free_list.push(index);
    Some(entry)
  }

  fn remove_borrowed_slot_ptr(&mut self, slot_ptr: *mut *mut u8) {
    for (idx, slot) in self.slots.iter_mut().enumerate() {
      let Some(entry) = slot.entry.as_ref() else {
        continue;
      };
      let Entry::Borrowed(ptr) = entry else {
        continue;
      };
      if *ptr != slot_ptr {
        continue;
      }
      let _ = slot.entry.take();
      slot.generation = slot.generation.wrapping_add(1);
      self.free_list.push(idx as u32);
    }
  }

  fn get_slot_ptr(&mut self, handle: u32) -> Option<*mut *mut u8> {
    let (index, generation) = decode_handle(handle)?;
    let slot = self.slots.get_mut(index as usize)?;
    if slot.generation != generation {
      return None;
    }
    let entry = slot.entry.as_mut()?;
    Some(entry.slot_ptr())
  }
}

/// Process-global root registry used by the runtime and the C ABI helpers.
pub fn global_root_registry() -> &'static RootRegistry {
  static GLOBAL: once_cell::sync::Lazy<RootRegistry> = once_cell::sync::Lazy::new(RootRegistry::new);
  &GLOBAL
}

/// A temporary root scope for runtime-native Rust code.
///
/// This is intended for protecting GC-managed pointers across operations in
/// runtime-native code that is not covered by LLVM stackmaps (e.g. Rust code
/// calling into the GC).
///
/// The scope is backed by a per-thread handle stack in the runtime thread
/// registry. When the scope is dropped, all roots pushed into it are removed.
#[must_use]
pub struct RootScope {
  base_len: usize,
  // Not Send/Sync: scopes are per-thread.
  _not_send: PhantomData<std::rc::Rc<()>>,
}

impl RootScope {
  /// Create a new root scope for the current thread.
  ///
  /// If the current thread is not registered with the runtime thread registry,
  /// this returns a no-op scope.
  pub fn new() -> Self {
    let base_len = crate::threading::registry::current_thread_state()
      .map(|t| t.handle_stack_len())
      .unwrap_or(0);
    Self {
      base_len,
      _not_send: PhantomData,
    }
  }

  /// Push a root slot into this scope.
  pub fn push(&mut self, slot: *mut *mut u8) {
    if slot.is_null() {
      std::process::abort();
    }
    if let Some(thread) = crate::threading::registry::current_thread_state() {
      thread.handle_stack_push(slot);
    }
  }
}

impl Drop for RootScope {
  fn drop(&mut self) {
    if let Some(thread) = crate::threading::registry::current_thread_state() {
      thread.handle_stack_truncate(self.base_len);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::{Arc, Barrier};
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  #[test]
  fn handle_api_alloc_load_store_free() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    let p1 = 0x1234usize as *mut u8;
    let h = crate::exports::rt_handle_alloc(p1);
    assert_eq!(crate::exports::rt_handle_load(h), p1);

    let p2 = 0x5678usize as *mut u8;
    crate::exports::rt_handle_store(h, p2);
    assert_eq!(crate::exports::rt_handle_load(h), p2);

    crate::exports::rt_handle_free(h);
    assert_eq!(crate::exports::rt_handle_load(h), std::ptr::null_mut());

    // Storing through a freed handle is a no-op.
    crate::exports::rt_handle_store(h, p1);
    assert_eq!(crate::exports::rt_handle_load(h), std::ptr::null_mut());

    // Out-of-range `u64` values must be treated as invalid.
    let invalid = (u32::MAX as u64) + 1;
    assert_eq!(crate::exports::rt_handle_load(invalid), std::ptr::null_mut());
    crate::exports::rt_handle_store(invalid, p1);
    crate::exports::rt_handle_free(invalid);
  }

  #[test]
  fn handle_api_stale_handle_is_rejected() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    let p1 = 0x1111usize as *mut u8;
    let h1 = crate::exports::rt_handle_alloc(p1);
    assert_eq!(crate::exports::rt_handle_load(h1), p1);
    crate::exports::rt_handle_free(h1);

    // Allocate again; the same slot should be reused with an incremented generation.
    let p2 = 0x2222usize as *mut u8;
    let h2 = crate::exports::rt_handle_alloc(p2);
    assert_eq!(crate::exports::rt_handle_load(h2), p2);
    assert_eq!(crate::exports::rt_handle_load(h1), std::ptr::null_mut());
    assert_ne!(h1, h2, "generation should change when a slot is reused");

    crate::exports::rt_handle_free(h2);
  }

  #[test]
  fn handle_api_multithreaded_stw_stress_no_deadlock() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    // Use multiple registered mutator threads so stop-the-world coordination is exercised.
    const N_THREADS: usize = 4;
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(N_THREADS + 1));

    let mut workers = Vec::new();
    for t in 0..N_THREADS {
      let stop = stop.clone();
      let start = start.clone();
      workers.push(std::thread::spawn(move || {
        crate::threading::register_current_thread(crate::threading::ThreadKind::Worker);
        start.wait();

        let mut i = 0usize;
        while !stop.load(Ordering::Relaxed) {
          let base = 0x1000usize + (t * 0x100) + (i & 0xff);
          let p1 = base as *mut u8;
          let p2 = (base ^ 0x55aa) as *mut u8;

          let h = crate::exports::rt_handle_alloc(p1);
          let _ = crate::exports::rt_handle_load(h);
          crate::exports::rt_handle_store(h, p2);
          let _ = crate::exports::rt_handle_load(h);
          crate::exports::rt_handle_free(h);

          // Cooperate with stop-the-world requests.
          crate::threading::safepoint_poll();

          i = i.wrapping_add(1);
        }

        crate::threading::unregister_current_thread();
      }));
    }

    start.wait();

    // Stop-the-world while worker threads are actively contending on the handle table lock.
    let stw_res = std::panic::catch_unwind(|| {
      for _ in 0..25 {
        crate::safepoint::with_world_stopped(|| {});
      }
    });

    stop.store(true, Ordering::Relaxed);
    for w in workers {
      w.join().unwrap();
    }

    if let Err(panic) = stw_res {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn root_registry_get_set_roundtrip_for_borrowed_slot() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    let registry = RootRegistry::new();
    let mut slot = 0xdeadusize as *mut u8;
    let handle = registry.register_root_slot(&mut slot as *mut *mut u8);
    assert_eq!(registry.get(handle), Some(0xdeadusize as *mut u8));

    assert!(registry.set(handle, 0xbeefusize as *mut u8));
    assert_eq!(registry.get(handle), Some(0xbeefusize as *mut u8));

    registry.unregister(handle);
    assert_eq!(registry.get(handle), None);
    assert!(!registry.set(handle, std::ptr::null_mut()));
  }

  #[test]
  fn global_root_registry_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();
    global_root_registry().clear_for_tests();

    // Stop-the-world handshakes can take much longer in debug builds (especially
    // under parallel test execution on multi-agent hosts). Keep release builds
    // strict, but give debug builds enough slack to avoid flaky timeouts.
    const TIMEOUT: Duration = if cfg!(debug_assertions) {
      Duration::from_secs(30)
    } else {
      Duration::from_secs(2)
    };

    std::thread::scope(|scope| {
      // Thread A holds the registry lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to register a root while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<u32>();
      let (c_finish_tx, c_finish_rx) = mpsc::channel::<()>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let reg = global_root_registry();
        let guard = reg.inner.lock();
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

      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();

        c_start_rx.recv().unwrap();

        let mut slot = core::ptr::null_mut::<u8>();
        let handle = global_root_registry().register_root_slot(&mut slot as *mut *mut u8);
        c_done_tx.send(handle).unwrap();

        c_finish_rx.recv().unwrap();
        global_root_registry().unregister(handle);
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
          panic!("thread C did not enter a GC-safe region while blocked on the root registry lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
      // Mark this thread as the STW coordinator so GC-aware locks can be acquired while the stop
      // epoch is active (root enumeration needs to lock the registry).
      let _coordinator = crate::threading::safepoint::enter_stop_the_world_coordinator();
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
        "world failed to stop within timeout; root registry lock contention must not block STW"
      );

      // Root enumeration must be able to lock the global registry while the world is stopped.
      //
      // This is the specific integration point used by `for_each_root_slot_world_stopped`. Wrap it
      // in a watchdog so a regression fails by timeout rather than hanging the test runner.
      let (enum_done_tx, enum_done_rx) = mpsc::channel::<()>();
      let watchdog = scope.spawn(move || {
        if enum_done_rx.recv_timeout(TIMEOUT).is_err() {
          crate::threading::safepoint::rt_gc_resume_world();
          panic!("global root enumeration deadlocked on the registry lock");
        }
      });
      global_root_registry().for_each_root_slot(|_| {});
      let _ = enum_done_tx.send(());
      watchdog.join().unwrap();

      // Resume the world so the contending registration can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      let handle = c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("root registration should complete after world is resumed");
      assert_ne!(handle, 0);

      c_finish_tx.send(()).unwrap();
    });
  }
}
