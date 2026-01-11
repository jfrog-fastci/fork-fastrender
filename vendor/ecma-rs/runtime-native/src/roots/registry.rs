use parking_lot::Mutex;
use std::marker::PhantomData;

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
  inner: Mutex<Inner>,
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

// SAFETY: The registry stores raw pointers as opaque values. All mutation and
// enumeration are synchronized via the `Mutex`, and the GC only reads/updates
// the pointed-to slots while the world is stopped.
unsafe impl Send for Inner {}

impl RootRegistry {
  pub fn new() -> Self {
    Self {
      inner: Mutex::new(Inner::default()),
    }
  }

  /// Register a root slot whose storage is owned by the caller.
  ///
  /// The returned handle must later be passed to [`RootRegistry::unregister`].
  pub fn register_root_slot(&self, slot: *mut *mut u8) -> u32 {
    if slot.is_null() {
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

  /// Enumerate all registered root slots.
  pub fn for_each_root_slot(&self, mut f: impl FnMut(*mut *mut u8)) {
    let mut inner = self.inner.lock();
    for slot in &mut inner.slots {
      let Some(entry) = slot.entry.as_mut() else {
        continue;
      };
      let slot_ptr: *mut *mut u8 = match entry {
        Entry::Borrowed(ptr) => *ptr,
        Entry::Pinned(b) => b.as_mut() as *mut *mut u8,
      };
      f(slot_ptr);
    }
  }

  /// Test-only helper to reset the process-global registry.
  pub(crate) fn clear_for_tests(&self) {
    let mut inner = self.inner.lock();
    inner.slots.clear();
    inner.free_list.clear();
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
