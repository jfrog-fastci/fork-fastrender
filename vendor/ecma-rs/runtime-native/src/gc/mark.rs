use std::mem;

use super::roots::RememberedSet;
use super::roots::RootSet;
use super::weak::process_global_weak_handles_major;
use super::weak::run_weak_cleanups;
use super::ObjHeader;
use super::Tracer;
use crate::gc::heap::GcHeap;

impl GcHeap {
  /// Perform a full-heap major collection using a mark-region algorithm over
  /// Immix blocks plus sweeping of the large-object space.
  ///
  /// This method begins with a [`GcHeap::collect_minor`] to ensure no nursery
  /// objects remain.
  ///
  /// # Stop-the-world requirement
  /// This GC is **stop-the-world**: the caller must ensure there are no
  /// concurrent mutators and that the provided root/remembered sets remain
  /// stable for the duration of the call.
  pub fn collect_major(&mut self, roots: &mut dyn RootSet, remembered: &mut dyn RememberedSet) {
    self.collect_minor(roots, remembered);
    self.stats.major_collections += 1;

    // Toggle the epoch so we can treat previous marks as "unmarked" without
    // clearing every object header.
    self.mark_epoch ^= 1;
    let epoch = self.mark_epoch;

    // Reset all Immix liveness maps.
    self.immix.clear_line_marks();

    {
      let mut marker = Marker {
        heap: self,
        epoch,
        worklist: Vec::new(),
      };

      roots.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });

      let mut root_handles = mem::take(&mut marker.heap.root_handles);
      root_handles.for_each_root_slot(&mut |slot| {
        marker.visit_slot(slot);
      });
      marker.heap.root_handles = root_handles;

      while let Some(obj) = marker.worklist.pop() {
        marker.visit_obj(obj);
      }
    }

    self.process_weak_handles_major(epoch);
    process_global_weak_handles_major(self, epoch);
    run_weak_cleanups(self);
    self.immix.finalize_after_marking();
    self.los.sweep(epoch);
  }
}

struct Marker<'a> {
  heap: &'a mut GcHeap,
  epoch: u8,
  worklist: Vec<*mut u8>,
}

impl Marker<'_> {
  fn mark_obj(&mut self, mut obj: *mut u8) {
    if obj.is_null() {
      return;
    }

    debug_assert!(
      !self.heap.is_in_nursery(obj),
      "major GC must not see nursery pointers (minor GC should run first)"
    );

    // SAFETY: `obj` is expected to be a valid heap object.
    unsafe {
      let header = &*(obj as *const ObjHeader);
      if header.is_forwarded() {
        obj = header.forwarding_ptr();
      }
    }

    // SAFETY: `obj` points to an `ObjHeader`.
    let already_marked = unsafe { (&*(obj as *const ObjHeader)).is_marked(self.epoch) };
    if already_marked {
      return;
    }

    // SAFETY: `obj` points to an `ObjHeader`.
    unsafe {
      let header = &mut *(obj as *mut ObjHeader);
      header.set_mark_epoch(self.epoch);

      let size = super::obj_size(obj);
      if self.heap.is_in_immix(obj) {
        self.heap.immix.set_lines_for_live_object(obj, size);
      } else {
        debug_assert!(self.heap.is_in_los(obj), "unknown heap object location");
      }
    }

    self.worklist.push(obj);
  }
}

impl Tracer for Marker<'_> {
  fn visit_slot(&mut self, slot: *mut *mut u8) {
    // SAFETY: `slot` originates from root enumeration or from a valid object
    // descriptor, so it is a valid pointer to a GC reference.
    let obj = unsafe { *slot };
    self.mark_obj(obj);
  }
}
