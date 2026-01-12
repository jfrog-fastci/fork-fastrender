use once_cell::sync::Lazy;
use std::ptr;

use super::GcHeap;
use crate::threading::GcAwareMutex;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WeakHandle(u64);

impl WeakHandle {
  const INDEX_MASK: u64 = 0xFFFF_FFFF;
  const GENERATION_SHIFT: u32 = 32;

  #[inline]
  fn new(index: u32, generation: u32) -> Self {
    Self(((generation as u64) << Self::GENERATION_SHIFT) | index as u64)
  }

  #[inline]
  fn index(self) -> u32 {
    (self.0 & Self::INDEX_MASK) as u32
  }

  #[inline]
  fn generation(self) -> u32 {
    (self.0 >> Self::GENERATION_SHIFT) as u32
  }

  #[inline]
  pub fn as_u64(self) -> u64 {
    self.0
  }

  #[inline]
  pub fn from_u64(raw: u64) -> Self {
    Self(raw)
  }
}

#[derive(Debug, Clone, Copy)]
struct WeakSlot {
  ptr: *mut u8,
  generation: u32,
  occupied: bool,
}

impl WeakSlot {
  #[inline]
  fn new(ptr: *mut u8) -> Self {
    Self {
      ptr,
      generation: 0,
      occupied: true,
    }
  }
}

#[derive(Debug, Default)]
pub struct WeakHandles {
  slots: Vec<WeakSlot>,
  free_list: Vec<u32>,
}

// SAFETY: `WeakHandles` is an index-based table of raw GC pointers. It has no interior mutability
// and can be moved across threads safely; concurrent access must be synchronized externally (e.g.
// via a mutex or by the GC's stop-the-world requirement).
unsafe impl Send for WeakHandles {}

impl WeakHandles {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn weak_add(&mut self, ptr: *mut u8) -> WeakHandle {
    if let Some(idx) = self.free_list.pop() {
      let slot = &mut self.slots[idx as usize];
      debug_assert!(!slot.occupied);
      slot.ptr = ptr;
      slot.occupied = true;
      WeakHandle::new(idx, slot.generation)
    } else {
      let idx = u32::try_from(self.slots.len()).expect("too many weak handles");
      self.slots.push(WeakSlot::new(ptr));
      WeakHandle::new(idx, 0)
    }
  }

  pub fn weak_get(&self, handle: WeakHandle) -> Option<*mut u8> {
    let idx = usize::try_from(handle.index()).ok()?;
    let slot = self.slots.get(idx)?;
    if !slot.occupied || slot.generation != handle.generation() {
      return None;
    }

    let ptr = slot.ptr;
    if ptr.is_null() {
      None
    } else {
      Some(ptr)
    }
  }

  pub fn weak_set(&mut self, handle: WeakHandle, ptr: *mut u8) {
    let Some(slot) = self.get_slot_mut(handle) else {
      return;
    };
    slot.ptr = ptr;
  }

  pub fn weak_remove(&mut self, handle: WeakHandle) {
    let idx = usize::try_from(handle.index()).ok();
    let Some(idx) = idx else {
      return;
    };
    let Some(slot) = self.slots.get_mut(idx) else {
      return;
    };
    if !slot.occupied || slot.generation != handle.generation() {
      return;
    }

    slot.ptr = ptr::null_mut();
    slot.occupied = false;
    slot.generation = slot.generation.wrapping_add(1);
    self.free_list.push(idx as u32);
  }

  fn get_slot_mut(&mut self, handle: WeakHandle) -> Option<&mut WeakSlot> {
    let idx = usize::try_from(handle.index()).ok()?;
    let slot = self.slots.get_mut(idx)?;
    if !slot.occupied || slot.generation != handle.generation() {
      return None;
    }
    Some(slot)
  }

  pub(crate) fn for_each_slot_mut(&mut self, mut f: impl FnMut(&mut *mut u8)) {
    for slot in &mut self.slots {
      if slot.occupied {
        f(&mut slot.ptr);
      }
    }
  }
}

static WEAK_CLEANUPS: Lazy<GcAwareMutex<Vec<fn(&mut GcHeap)>>> = Lazy::new(|| GcAwareMutex::new(Vec::new()));

pub fn register_weak_cleanup(f: fn(&mut GcHeap)) {
  WEAK_CLEANUPS.lock().push(f);
}

pub(crate) fn run_weak_cleanups(heap: &mut GcHeap) {
  // GC must not allocate, so avoid cloning the Vec. Instead, copy out one function pointer at a
  // time under the mutex, then invoke it after releasing the lock.
  let mut idx = 0usize;
  loop {
    let Some(cleanup) = WEAK_CLEANUPS.lock_for_gc().get(idx).copied() else {
      break;
    };
    cleanup(heap);
    idx += 1;
  }
}
static GLOBAL_WEAK_HANDLES: Lazy<GcAwareMutex<WeakHandles>> =
  Lazy::new(|| GcAwareMutex::new(WeakHandles::new()));

pub(crate) fn global_weak_add(ptr: *mut u8) -> WeakHandle {
  GLOBAL_WEAK_HANDLES.lock().weak_add(ptr)
}

/// Like [`global_weak_add`], but reads the pointer value from an addressable slot after acquiring
/// the weak-handle table lock.
///
/// This is intended for moving-GC-safe exported entrypoints that accept GC pointers as `GcHandle`
/// (pointer-to-slot) arguments.
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a writable `*mut u8` slot.
pub(crate) unsafe fn global_weak_add_from_slot(slot: *mut *mut u8) -> WeakHandle {
  let mut table = GLOBAL_WEAK_HANDLES.lock();
  if slot.is_null() {
    std::process::abort();
  }
  if (slot as usize) % core::mem::align_of::<*mut u8>() != 0 {
    std::process::abort();
  }
  // SAFETY: caller contract.
  let ptr = unsafe { slot.read() };
  table.weak_add(ptr)
}

pub(crate) fn global_weak_get(handle: WeakHandle) -> Option<*mut u8> {
  GLOBAL_WEAK_HANDLES.lock().weak_get(handle)
}

pub(crate) fn global_weak_remove(handle: WeakHandle) {
  GLOBAL_WEAK_HANDLES.lock().weak_remove(handle);
}

/// Debug/test helper: snapshot the global weak-handle table sizes.
///
/// Returns `(slots_len, free_list_len)`.
///
/// This is intentionally not part of the stable runtime-native ABI; it exists so integration tests
/// can assert that weak handles are reclaimed and reused after GC cycles.
#[doc(hidden)]
pub fn debug_global_weak_handles_table_sizes() -> (usize, usize) {
  let table = GLOBAL_WEAK_HANDLES.lock();
  (table.slots.len(), table.free_list.len())
}

/// Debug/test helper: lock the process-global weak-handle table.
///
/// This is used by integration tests to force lock contention while exercising stop-the-world GC
/// coordination. It is not considered stable API.
#[doc(hidden)]
pub fn debug_hold_global_weak_handles_lock() -> impl Drop {
  struct Hold {
    _guard: parking_lot::MutexGuard<'static, WeakHandles>,
  }

  impl Drop for Hold {
    fn drop(&mut self) {}
  }

  Hold {
    _guard: GLOBAL_WEAK_HANDLES.lock(),
  }
}

pub(crate) fn process_global_weak_handles_minor(heap: &GcHeap) {
  let mut handles = GLOBAL_WEAK_HANDLES.lock_for_gc();

  handles.for_each_slot_mut(|slot| {
    let obj = *slot;
    if obj.is_null() {
      return;
    }

    if heap.is_in_nursery(obj) {
      // SAFETY: `obj` is expected to point at the start of a nursery object.
      unsafe {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          *slot = header.forwarding_ptr();
        } else {
          *slot = ptr::null_mut();
        }
      }
    }
  });
}

pub(crate) fn process_global_weak_handles_major(heap: &GcHeap, epoch: u8) {
  let mut handles = GLOBAL_WEAK_HANDLES.lock_for_gc();

  handles.for_each_slot_mut(|slot| {
    let mut obj = *slot;
    if obj.is_null() {
      return;
    }

    if heap.is_in_nursery(obj) {
      // Major GC should not see nursery pointers (it runs a minor GC first), but handle them
      // defensively.
      // SAFETY: `obj` is expected to point at the start of a nursery object.
      unsafe {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
        } else {
          *slot = ptr::null_mut();
          return;
        }
      }
    }

    // Only attempt to inspect heap headers for objects known to belong to this heap.
    // (The exported weak-handle API is process-global; other runtimes/tests may store pointers from
    // other heaps.)
    if !heap.is_in_immix(obj) && !heap.is_in_los(obj) {
      return;
    }

    // Follow forwarding pointers (used by nursery evacuation today, and by potential future major
    // GC compaction).
    // SAFETY: `obj` is expected to point at the start of a heap object.
    unsafe {
      loop {
        let header = &*super::header_from_obj(obj);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
        } else {
          break;
        }
      }

      let header = &*super::header_from_obj(obj);
      if header.is_marked(epoch) {
        *slot = obj;
      } else {
        *slot = ptr::null_mut();
      }
    }
  });
}
