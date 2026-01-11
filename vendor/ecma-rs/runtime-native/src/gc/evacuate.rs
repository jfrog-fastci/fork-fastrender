use std::mem;
use std::ptr;
use std::time::Instant;

use super::roots::RememberedSet;
use super::roots::RootSet;
use super::weak::process_global_weak_handles_minor;
use super::weak::run_weak_cleanups;
use super::ObjHeader;
use super::Tracer;
use crate::gc::heap::GcHeap;

impl GcHeap {
  /// Perform a minor collection (nursery evacuation).
  ///
  /// # Stop-the-world requirement
  /// This GC is **stop-the-world**: the caller must ensure there are no
  /// concurrent mutators and that the provided root/remembered sets remain
  /// stable for the duration of the call.
  pub fn collect_minor(&mut self, roots: &mut dyn RootSet, remembered: &mut dyn RememberedSet) {
    let start = Instant::now();
    self.stats.minor_collections += 1;

    // Snapshot nursery usage so we can optionally poison the previously-used
    // region after `reset` (helps surface stale nursery pointers quickly).
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    let nursery_poison_len = self.nursery.allocated_bytes();

    {
      let mut evac = Evacuator {
        heap: self,
      };

      roots.for_each_root_slot(&mut |slot| {
        evac.visit_slot(slot);
      });

      let mut root_handles = mem::take(&mut evac.heap.root_handles);
      root_handles.for_each_root_slot(&mut |slot| {
        evac.visit_slot(slot);
      });
      evac.heap.root_handles = root_handles;

      remembered.for_each_remembered_obj(&mut |obj| {
        evac.visit_obj(obj);
      });
    }

    // All nursery pointers reachable from roots/remembered objects should now be
    // forwarded to old-gen.
    self.process_finalizers_minor();
    self.process_weak_handles_minor();
    process_global_weak_handles_minor(self);
    self.nursery_tlab.clear();
    // SAFETY: `collect_minor` is documented as stop-the-world; there must be no
    // concurrent mutators or allocations when resetting the nursery.
    unsafe {
      self.nursery.reset();
    }
    #[cfg(any(debug_assertions, feature = "gc_debug"))]
    unsafe {
      ptr::write_bytes(self.nursery.start(), 0xDD, nursery_poison_len);
    }
    remembered.clear();
    run_weak_cleanups(self);

    let pause = start.elapsed();
    self.stats.last_minor_pause = pause;
    self.stats.total_minor_pause += pause;
  }
}

struct Evacuator<'a> {
  heap: &'a mut GcHeap,
}

impl Evacuator<'_> {
  fn evacuate(&mut self, obj: *mut u8) -> (*mut u8, bool) {
    debug_assert!(self.heap.is_in_nursery(obj));

    // SAFETY: `obj` is a valid GC object in the nursery.
    unsafe {
      let header = &mut *(obj as *mut ObjHeader);
      if header.is_forwarded() {
        return (header.forwarding_ptr(), false);
      }

      let size = super::obj_size(obj);

      let new_obj = self.heap.alloc_old_raw(size, mem::align_of::<ObjHeader>());

      ptr::copy_nonoverlapping(obj, new_obj, size);
      header.set_forwarding_ptr(new_obj);
      #[cfg(any(debug_assertions, feature = "gc_debug"))]
      {
        // Minor GC must not allocate; validate evacuation on the fly instead of building a list.
        self.heap.verify_forwarding_pairs(&[(obj, new_obj)]);
      }

      (new_obj, true)
    }
  }
}

impl Tracer for Evacuator<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let obj = unsafe { *slot };
    if obj.is_null() {
      return;
    }

    if self.heap.is_in_nursery(obj) {
      let (new_obj, is_new) = self.evacuate(obj);
      // SAFETY: `slot` is valid and writable.
      unsafe {
        *slot = new_obj;
      }
      if is_new {
        // Depth-first evacuation: immediately scan the promoted copy to avoid allocating a worklist
        // (GC must not allocate).
        self.visit_obj(new_obj);
      }
    }
  }
}
