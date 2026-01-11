use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::ptr;

use super::GcHeap;

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
    if ptr.is_null() { None } else { Some(ptr) }
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

static WEAK_CLEANUPS: Lazy<Mutex<Vec<fn(&mut GcHeap)>>> = Lazy::new(|| Mutex::new(Vec::new()));

pub fn register_weak_cleanup(f: fn(&mut GcHeap)) {
  WEAK_CLEANUPS.lock().push(f);
}

pub(crate) fn run_weak_cleanups(heap: &mut GcHeap) {
  // Copy out the current list so we don't execute arbitrary callbacks while
  // holding the global lock.
  let cleanups = WEAK_CLEANUPS.lock().clone();
  for cleanup in cleanups {
    cleanup(heap);
  }
}

