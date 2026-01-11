use super::for_each_ptr_slot;
use super::heap::GcHeap;
use super::ObjHeader;
use super::RootSet;
use super::TypeDescriptor;
use ahash::AHashSet;
use once_cell::sync::Lazy;
use crate::sync::GcAwareMutex;
use std::mem;

/// Registry of type descriptors we've seen during allocation.
///
/// This lets debug verification reject corrupted `type_desc` pointers *without*
/// dereferencing them (which would be UB if the pointer is invalid).
static KNOWN_TYPE_DESCRIPTORS: Lazy<GcAwareMutex<AHashSet<usize>>> =
  Lazy::new(|| GcAwareMutex::new(AHashSet::new()));

pub(crate) fn register_type_descriptor(desc: &'static TypeDescriptor) {
  register_type_descriptor_ptr(desc as *const TypeDescriptor);
}

pub(crate) fn register_type_descriptor_ptr(desc: *const TypeDescriptor) {
  KNOWN_TYPE_DESCRIPTORS
    .lock()
    .insert(desc as usize);
}

pub(crate) fn is_known_type_descriptor(desc: *const TypeDescriptor) -> bool {
  if desc.is_null() {
    return false;
  }
  KNOWN_TYPE_DESCRIPTORS.lock().contains(&(desc as usize))
}

impl GcHeap {
  /// Expensive verifier intended for tests and fuzzing.
  ///
  /// Invariants checked (high-level):
  /// - All roots point to null or a valid object in one of the heap spaces.
  /// - Every pointer slot in every reachable object points to null or a valid object.
  /// - No reachable pointer points into *unallocated* nursery memory (so after a minor
  ///   collection, when the nursery has been reset, there must be no nursery pointers).
  pub fn verify_from_roots(&self, roots: &mut dyn RootSet) {
    let nursery_base = self.nursery.start() as usize;
    let nursery_alloc_end = nursery_base + self.nursery.allocated_bytes();
    let min_align = super::OBJ_ALIGN;

    let known_desc = KNOWN_TYPE_DESCRIPTORS.lock();

    let mut worklist: Vec<*mut u8> = Vec::new();
    roots.for_each_root_slot(&mut |slot| {
      // SAFETY: `slot` comes from a `RootSet` implementation; the contract says it is a valid
      // pointer to a GC reference and may be updated in-place.
      let obj = unsafe { *slot };
      if obj.is_null() {
        return;
      }
      self.verify_obj_ptr(obj, nursery_base, nursery_alloc_end, min_align, &known_desc);
      worklist.push(obj);
    });

    let mut seen: AHashSet<usize> = AHashSet::new();
    while let Some(mut obj) = worklist.pop() {
      // Handle forwarding transparently: verification should operate on the actual object body.
      // SAFETY: `obj` is an in-heap pointer validated by `verify_obj_ptr` above.
      unsafe {
        let header = &*(obj as *const ObjHeader);
        if header.is_forwarded() {
          obj = header.forwarding_ptr();
          self.verify_obj_ptr(obj, nursery_base, nursery_alloc_end, min_align, &known_desc);
        }
      }

      if !seen.insert(obj as usize) {
        continue;
      }

      // SAFETY: `obj` is a valid heap object.
      unsafe {
        for_each_ptr_slot(obj, |slot| {
          let child = *slot;
          if child.is_null() {
            return;
          }
          self.verify_obj_ptr(child, nursery_base, nursery_alloc_end, min_align, &known_desc);
          worklist.push(child);
        });
      }
    }
  }

  pub(crate) fn verify_forwarding_pairs(&self, forwarded: &[(*mut u8, *mut u8)]) {
    let min_align = super::OBJ_ALIGN;
    let known_desc = KNOWN_TYPE_DESCRIPTORS.lock();

    for &(from, to) in forwarded {
      assert!(!from.is_null(), "forwarded-from pointer is null");
      assert!(!to.is_null(), "forwarded-to pointer is null");
      assert!(
        self.is_in_nursery(from),
        "forwarded-from pointer is not in nursery: {:#x}",
        from as usize
      );
      assert_eq!(
        (from as usize) & (min_align - 1),
        0,
        "forwarded-from pointer is misaligned: {:#x}",
        from as usize
      );

      // SAFETY: `from` points into nursery memory and was recorded during evacuation.
      let from_header = unsafe { &*(from as *const ObjHeader) };
      self.verify_obj_header(from_header, &known_desc);
      let from_desc = unsafe { &*from_header.type_desc };
      assert!(
        from_desc.align.is_power_of_two(),
        "type descriptor has non-power-of-two alignment: {}",
        from_desc.align
      );
      assert_eq!(
        (from as usize) & (from_desc.align - 1),
        0,
        "forwarded-from pointer does not satisfy its descriptor alignment ({}): {:#x}",
        from_desc.align,
        from as usize
      );
      assert!(
        from_header.is_forwarded(),
        "evacuated nursery object is not marked as forwarded"
      );
      assert_eq!(
        from_header.forwarding_ptr(),
        to,
        "forwarding pointer does not match recorded target"
      );

      assert!(
        !self.is_in_nursery(to),
        "forward target unexpectedly points back into nursery"
      );
      assert!(
        self.is_in_immix(to) || self.is_in_los(to),
        "forward target is not in old/LOS"
      );
      assert_eq!(
        (to as usize) & (from_desc.align - 1),
        0,
        "forwarded-to pointer does not satisfy its descriptor alignment ({}): {:#x}",
        from_desc.align,
        to as usize
      );
      // SAFETY: `to` was allocated by the GC.
      let to_header = unsafe { &*(to as *const ObjHeader) };
      self.verify_obj_header(to_header, &known_desc);
      assert!(
        !to_header.is_forwarded(),
        "forward target unexpectedly marked as forwarded"
      );
      assert_eq!(
        to_header.type_desc, from_header.type_desc,
        "forward target type descriptor mismatch"
      );
    }
  }

  fn verify_obj_ptr(
    &self,
    obj: *mut u8,
    nursery_base: usize,
    nursery_alloc_end: usize,
    min_align: usize,
    known_desc: &AHashSet<usize>,
  ) {
    let addr = obj as usize;
    assert_eq!(
      addr & (min_align - 1),
      0,
      "GC pointer is misaligned: {addr:#x}"
    );

    if self.is_in_nursery(obj) {
      assert!(
        addr >= nursery_base && addr < nursery_alloc_end,
        "GC pointer points into unallocated nursery memory: {addr:#x} (nursery_used={:#x})",
        nursery_alloc_end
      );
    } else {
      assert!(
        self.is_in_immix(obj) || self.is_in_los(obj),
        "GC pointer is not in any heap space: {addr:#x}"
      );
    }

    // SAFETY: `obj` points into one of the heap spaces.
    let header = unsafe { &*(obj as *const ObjHeader) };
    self.verify_obj_header(header, known_desc);
    if header.is_pinned() {
      assert!(
        self.is_in_los(obj),
        "pinned object is not in LOS (policy: pinned objects must always be allocated in LOS)"
      );
    }

    let desc = unsafe { &*header.type_desc };
    let size = unsafe { super::obj_size(obj) };
    assert!(desc.size >= mem::size_of::<ObjHeader>(), "type descriptor size too small");
    assert!(size >= mem::size_of::<ObjHeader>(), "object size too small");
    assert!(
      size >= desc.size,
      "object size {size} smaller than descriptor size {}",
      desc.size
    );
    assert!(
      desc.align != 0 && desc.align.is_power_of_two(),
      "type descriptor has non-power-of-two alignment: {}",
      desc.align
    );
    assert_eq!(
      addr & (desc.align - 1),
      0,
      "GC pointer does not satisfy its descriptor alignment ({}): {addr:#x}",
      desc.align
    );

    // Only nursery objects have a contiguous allocated range we can bounds-check.
    if self.is_in_nursery(obj) {
      assert!(
        addr + size <= nursery_alloc_end,
        "nursery object overruns nursery allocation range"
      );
    }
  }

  fn verify_obj_header(&self, header: &ObjHeader, known_desc: &AHashSet<usize>) {
    assert!(
      !header.type_desc.is_null(),
      "object has null type descriptor pointer"
    );
    assert!(
      known_desc.contains(&(header.type_desc as usize)),
      "object has unknown/corrupt type descriptor pointer: {:#x}",
      header.type_desc as usize
    );

    // SAFETY: the pointer is known-good (registered during allocation).
    let desc = unsafe { &*header.type_desc };
    for &offset_u32 in desc.ptr_offsets() {
      let offset = offset_u32 as usize;
      assert_eq!(
        offset % mem::align_of::<*mut u8>(),
        0,
        "type descriptor contains misaligned pointer offset"
      );
      assert!(
        offset + mem::size_of::<*mut u8>() <= desc.size,
        "type descriptor pointer offset out of bounds"
      );
    }

    if header.is_forwarded() {
      let fwd = header.forwarding_ptr();
      assert!(
        !fwd.is_null(),
        "forwarded object has null forwarding pointer"
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::threading;
  use crate::threading::ThreadKind;
  use std::sync::mpsc;
  use std::time::Duration;
  use std::time::Instant;

  static TEST_DESC: TypeDescriptor = TypeDescriptor::new(16, &[]);

  #[test]
  fn known_type_descriptors_lock_is_gc_aware() {
    let _rt = crate::test_util::TestRuntimeGuard::new();

    const TIMEOUT: Duration = Duration::from_secs(2);

    std::thread::scope(|scope| {
      // Thread A holds the descriptor registry lock.
      let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
      let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

      // Thread C attempts to register a descriptor while the lock is held.
      let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
      let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
      let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

      scope.spawn(move || {
        threading::register_current_thread(ThreadKind::Worker);
        let guard = KNOWN_TYPE_DESCRIPTORS.lock();
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
        drop(guard);

        // Cooperatively stop at the safepoint request.
        crate::rt_gc_safepoint();
        threading::unregister_current_thread();
      });

      a_locked_rx
        .recv_timeout(TIMEOUT)
        .expect("thread A should acquire the descriptor registry lock");

      scope.spawn(move || {
        let id = threading::register_current_thread(ThreadKind::Worker);
        c_registered_tx.send(id).unwrap();
        c_start_rx.recv().unwrap();

        register_type_descriptor_ptr(&TEST_DESC as *const TypeDescriptor);
        c_done_tx.send(()).unwrap();

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
          panic!("thread C did not enter a GC-safe region while blocked on the descriptor registry lock");
        }
        std::thread::yield_now();
      }

      // Request a stop-the-world GC and ensure it can complete even though thread C is blocked.
      let stop_epoch = crate::threading::safepoint::rt_gc_try_request_stop_the_world()
        .expect("stop-the-world should not already be active");
      assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
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
        "world failed to stop within timeout; descriptor registry lock contention must not block STW"
      );

      // Resume the world so the contending registration can complete.
      crate::threading::safepoint::rt_gc_resume_world();

      c_done_rx
        .recv_timeout(TIMEOUT)
        .expect("descriptor registration should complete after world is resumed");
    });
  }
}
